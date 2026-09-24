// SPDX-License-Identifier: GPL-2.0-or-later
#![cfg(target_os = "android")]

//! The MediaCodec decoder, driven from Rust via the `ndk` crate.
//!
//! Configured for Surface output (never a `TextureView`) with the low-latency
//! keys, fed the server's `csd-0` and then the reassembled access units, and
//! released with a presentation timestamp so the compositor shows each frame on
//! a vsync rather than immediately (the microstutter trap in CLAUDE.md).

use std::time::Duration;

use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
    MediaFormat,
};
use ndk::native_window::NativeWindow;
use sunburst_core::instr::{self, Stage};
use sunburst_core::proto::{CodecPrivate, StreamCodec};

use crate::hdr_static_info::hdr_static_info;
use crate::reconfig::media_color_keys;

/// A configured, started decoder rendering to a Surface.
pub struct Decoder {
    codec: MediaCodec,
}

fn mime(codec: StreamCodec) -> &'static str {
    match codec {
        StreamCodec::Hevc => "video/hevc",
        StreamCodec::Av1 => "video/av01",
        StreamCodec::H264 => "video/avc",
    }
}

/// The format one encoder build needs: its size and `csd-0` (HEVC/H.264
/// parameter sets in Annex-B, or the AV1 av1C record), the low-latency keys,
/// and its colour. The colour keys are set for SDR too, explicitly BT.709, so a
/// decoder never guesses; HDR adds the ST 2086 static info when the build
/// carries it (it rides in the bitstream as well).
fn format_for(cp: &CodecPrivate, fps: i32) -> MediaFormat {
    let mut fmt = MediaFormat::new();
    fmt.set_str("mime", mime(cp.codec));
    fmt.set_i32("width", i32::from(cp.width));
    fmt.set_i32("height", i32::from(cp.height));
    fmt.set_buffer("csd-0", &cp.data);
    // Low-latency decode: no reorder buffering. KEY_LOW_LATENCY landed at
    // API 30, this project's floor. Priority 0 = realtime; a high operating
    // rate tells the codec to run flat out.
    fmt.set_i32("low-latency", 1);
    fmt.set_i32("priority", 0);
    fmt.set_i32("operating-rate", fps.max(60) * 2);

    if let Some((standard, transfer, range)) = media_color_keys(&cp.color) {
        fmt.set_i32("color-standard", standard);
        fmt.set_i32("color-transfer", transfer);
        fmt.set_i32("color-range", range);
    }
    if cp.color.is_pq()
        && let Some(m) = &cp.hdr
    {
        fmt.set_buffer("hdr-static-info", &hdr_static_info(m));
    }
    fmt
}

impl Decoder {
    /// Build and start a decoder for one encoder build, rendering into
    /// `surface`.
    pub fn new(cp: &CodecPrivate, fps: i32, surface: &NativeWindow) -> Result<Decoder, String> {
        let mime = mime(cp.codec);
        let codec =
            MediaCodec::from_decoder_type(mime).ok_or_else(|| format!("no decoder for {mime}"))?;
        codec
            .configure(
                &format_for(cp, fps),
                Some(surface),
                MediaCodecDirection::Decoder,
            )
            .map_err(|e| format!("configure: {e}"))?;
        codec.start().map_err(|e| format!("start: {e}"))?;
        Ok(Decoder { codec })
    }

    /// Reconfigure for a new build of the same codec: stop, configure, start,
    /// on the same codec instance and surface. A fresh instance if that fails.
    ///
    /// This allocates (a `MediaFormat`, the codec's buffers). It is the client's
    /// twin of the server rebuilding its encoder, and happens only when a build
    /// changed what the decoder sees: an HDR↔SDR flip or a new capture size.
    pub fn reconfigure(
        &mut self,
        cp: &CodecPrivate,
        fps: i32,
        surface: &NativeWindow,
    ) -> Result<(), String> {
        let _ = self.codec.stop();
        let reused = self
            .codec
            .configure(
                &format_for(cp, fps),
                Some(surface),
                MediaCodecDirection::Decoder,
            )
            .and_then(|()| self.codec.start());
        if reused.is_err() {
            *self = Decoder::new(cp, fps, surface)?;
        }
        Ok(())
    }

    /// Queue one access unit for decode, tagged `pts_us`. Returns `false` if no
    /// input buffer was free (the frame is dropped; the next keyframe or a NACK
    /// recovers it). Records [`Stage::DecodeSubmit`].
    pub fn feed(&self, data: &[u8], pts_us: u64, frame_id: u32) -> Result<bool, String> {
        match self
            .codec
            .dequeue_input_buffer(Duration::ZERO)
            .map_err(|e| format!("dequeue_input: {e}"))?
        {
            DequeuedInputBufferResult::Buffer(mut buf) => {
                let dst = buf.buffer_mut();
                if dst.len() < data.len() {
                    return Err(format!("input buffer {} < frame {}", dst.len(), data.len()));
                }
                for (d, s) in dst.iter_mut().zip(data) {
                    d.write(*s);
                }
                instr::record(Stage::DecodeSubmit, frame_id);
                self.codec
                    .queue_input_buffer(buf, 0, data.len(), pts_us, 0)
                    .map_err(|e| format!("queue_input: {e}"))?;
                Ok(true)
            }
            DequeuedInputBufferResult::TryAgainLater => Ok(false),
        }
    }

    /// Drain any ready output, releasing each frame for presentation at
    /// `present_ns` (a CLOCK_MONOTONIC timestamp; the compositor shows it on the
    /// next vsync at/after that time). Records [`Stage::DecodeOut`]/[`Stage::Present`].
    pub fn drain(&self, present_ns: i64, frame_id: u32) -> Result<(), String> {
        loop {
            match self
                .codec
                .dequeue_output_buffer(Duration::ZERO)
                .map_err(|e| format!("dequeue_output: {e}"))?
            {
                DequeuedOutputBufferInfoResult::Buffer(out) => {
                    instr::record(Stage::DecodeOut, frame_id);
                    self.codec
                        .release_output_buffer_at_time(out, present_ns)
                        .map_err(|e| format!("release_output: {e}"))?;
                    instr::record(Stage::Present, frame_id);
                }
                // No frame ready, or a format/buffer change we do not act on in
                // Surface mode.
                DequeuedOutputBufferInfoResult::TryAgainLater => break,
                DequeuedOutputBufferInfoResult::OutputFormatChanged
                | DequeuedOutputBufferInfoResult::OutputBuffersChanged => {}
            }
        }
        Ok(())
    }

    pub fn stop(&self) {
        let _ = self.codec.stop();
    }
}

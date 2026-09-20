// SPDX-License-Identifier: GPL-2.0-or-later

//! Framing an Xbox headset microphone into `AudioIn` Opus packets.
//!
//! [`MicFramer`] is the pure half of the client mic path: one headset's worth of
//! encode state — an [`OpusEncoder`], a [`FrameAccumulator`] and a sequence
//! counter. The driver hands mic PCM in whatever chunk sizes its USB transfers
//! deliver; this regroups them into whole 5 ms frames, encodes each, and emits
//! it with an incrementing sequence for the wire.
//!
//! It never names the GIP bridge or the socket, so — like [`crate::pad`] and
//! [`crate::input_map`] — it is not `#[cfg(target_os = "android")]` and is
//! host-tested. The Android-gated [`crate::audio::MicCapture`] owns one of these
//! per pad and drives it from `bridge.audio_in`.

use sunburst_audio::codec::{Application, OpusEncoder};
use sunburst_audio::pcm::FrameAccumulator;
use sunburst_core::proto::{MAX_PAYLOAD, Seq16};

/// Opus frame granularity for the mic: 5 ms, short to keep added latency down.
/// The sample count per channel is `rate * 5 / 1000` (120 at 24 kHz, 240 at
/// 48 kHz) — all valid Opus frame sizes. The server regroups regardless.
fn samples_per_ch(rate: u32) -> usize {
    (rate as usize) * 5 / 1000
}

/// One headset microphone's encode pipeline, at the mic's native rate/channels.
pub struct MicFramer {
    encoder: OpusEncoder,
    acc: FrameAccumulator,
    rate: u32,
    channels: usize,
    seq: u16,
    /// Scratch for one whole interleaved frame and its encoded Opus packet, so
    /// draining does not allocate after construction.
    frame: Vec<i16>,
    opus: Vec<u8>,
}

impl MicFramer {
    /// Build a framer for the mic's native format — `rate` Hz, `channels`
    /// (1 mono / 2 stereo) — at `bitrate_kbps`. The encoder runs at the native
    /// rate; the server's 48 kHz Opus decoder resamples on decode.
    /// `Application::LowDelay` matches the rest of the low-latency audio path.
    pub fn new(rate: u32, channels: usize, bitrate_kbps: u32) -> Result<MicFramer, String> {
        let mut encoder = OpusEncoder::new(rate, channels as u8, Application::LowDelay)
            .map_err(|e| format!("mic Opus encoder: {e}"))?;
        encoder
            .set_bitrate(bitrate_kbps.saturating_mul(1000))
            .map_err(|e| format!("mic bitrate: {e}"))?;
        let spc = samples_per_ch(rate);
        Ok(MicFramer {
            encoder,
            acc: FrameAccumulator::new(spc, channels),
            rate,
            channels,
            seq: 0,
            frame: vec![0i16; spc * channels],
            opus: vec![0u8; MAX_PAYLOAD],
        })
    }

    /// The rate and channel count this framer was built for, so the caller can
    /// rebuild it if the headset renegotiates its capture format.
    pub fn format(&self) -> (u32, usize) {
        (self.rate, self.channels)
    }

    /// Push interleaved mic PCM and emit every whole frame it completes, each as
    /// `(sequence, Opus packet)`. A partial trailing frame carries to the next
    /// call. An encode error drops that one frame rather than desyncing the seq.
    pub fn push_and_drain(&mut self, pcm: &[i16], mut emit: impl FnMut(Seq16, &[u8])) {
        self.acc.push(pcm);
        while self.acc.pop_frame(&mut self.frame) {
            if let Ok(len) = self.encoder.encode(&self.frame, &mut self.opus) {
                emit(Seq16(self.seq), &self.opus[..len]);
                self.seq = self.seq.wrapping_add(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regroups_odd_chunks_into_frames_with_increasing_seq() {
        // 24 kHz mono — the real Xbox chat mic; a 5 ms frame is 120 samples.
        let mut f = MicFramer::new(24_000, 1, 32).expect("framer");
        assert_eq!(f.format(), (24_000, 1));
        let mut got: Vec<(Seq16, Vec<u8>)> = Vec::new();
        // 150 mono samples buffered → one 120-frame out, 30 left.
        f.push_and_drain(&vec![0i16; 150], |seq, opus| got.push((seq, opus.to_vec())));
        assert_eq!(got.len(), 1, "one whole 5 ms frame after 150 samples");
        // +150 (=180 buffered) → one more frame, 60 left.
        f.push_and_drain(&vec![0i16; 150], |seq, opus| got.push((seq, opus.to_vec())));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, Seq16(0));
        assert_eq!(got[1].0, Seq16(1), "sequence increments per frame");
        assert!(got.iter().all(|(_, o)| !o.is_empty()), "each frame encodes");
    }

    #[test]
    fn stereo_48k_frames_take_twice_the_samples() {
        // 48 kHz stereo — a 5 ms frame is 240 samples/ch.
        let mut f = MicFramer::new(48_000, 2, 32).expect("framer");
        let mut count = 0usize;
        f.push_and_drain(&vec![0i16; 240 * 2], |_, _| count += 1);
        assert_eq!(count, 1, "exactly one stereo 5 ms frame");
        // Half a frame more emits nothing.
        f.push_and_drain(&vec![0i16; 240], |_, _| count += 1);
        assert_eq!(count, 1, "a partial frame is held, not emitted");
    }

    #[test]
    fn nothing_is_emitted_below_a_full_frame() {
        let mut f = MicFramer::new(24_000, 1, 24).expect("framer");
        let mut count = 0usize;
        f.push_and_drain(&[0i16; 100], |_, _| count += 1); // < 120
        assert_eq!(count, 0);
    }
}

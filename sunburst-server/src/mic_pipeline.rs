// SPDX-License-Identifier: GPL-2.0-or-later

//! The pad-mic receive path: Opus in → decode → mix → WASAPI render, on its own
//! thread. The inbound mirror of [`crate::audio_pipeline`].
//!
//! A paired pad's headset microphone arrives as `AudioIn` datagrams (one Opus
//! frame each, tagged with the pad index). The endpoint hands each frame to the
//! `SessionManager`, which forwards it here; this thread decodes it, mixes the
//! active pads together, and renders the result into a **consumed virtual
//! microphone** — Steam's signed "Steam Streaming Microphone" endpoint, selected
//! by name — so games read the pad mic as an input device. No driver of ours is
//! installed; it is the same "consume a signed device" posture the loopback
//! capture side takes with "Steam Streaming Speakers".
//!
//! Off the video frame path and low-rate (~a frame every 5–20 ms per pad), so it
//! is allowed the small per-frame allocation the channel carries — the hot-path
//! zero-alloc rule is about capture→encode→send, which this is not. Decode and
//! render run here, never on the endpoint's receive thread.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use sunburst_audio::RenderPlayback;
use sunburst_audio::codec::OpusDecoder;
use sunburst_audio::pcm::FrameAccumulator;
use sunburst_core::proto::input::MAX_PADS;

use crate::realtime::RealtimeThread;

const MAX_PADS_USIZE: usize = MAX_PADS as usize;

/// Largest Opus frame we might decode (120 ms at 48 kHz), per channel.
const MAX_FRAME_SAMPLES: usize = 5760;

/// Mix granularity: 5 ms of 48 kHz stereo. Whatever Opus frame size the client
/// used, the per-pad accumulators regroup to this before mixing.
const MIX_SAMPLES_PER_CH: usize = 240;
const MIX_FRAME_LEN: usize = MIX_SAMPLES_PER_CH * 2;

/// One decoded-and-tagged frame handed from the endpoint thread to the mixer.
struct MicFrame {
    pad: u8,
    opus: Vec<u8>,
}

/// A running pad-mic pipeline: the channel frames are pushed onto, and the mixer
/// thread's join handle. Dropping it closes the channel and joins the thread.
pub struct MicPipeline {
    tx: Option<Sender<MicFrame>>,
    thread: Option<JoinHandle<()>>,
}

impl MicPipeline {
    /// Spawn the mixer/render thread for `device` (a virtual-mic endpoint name).
    /// Always returns a handle; if the endpoint cannot be opened the thread logs
    /// and exits, and pushed frames are silently dropped.
    pub fn spawn(device: String) -> MicPipeline {
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("sunburst-mic".into())
            .spawn(move || mic_loop(device, rx))
            .ok();
        MicPipeline {
            tx: Some(tx),
            thread,
        }
    }

    /// Forward one pad-mic Opus frame to the mixer. Non-blocking; a send to a
    /// thread that has exited (no endpoint) is dropped.
    pub fn push(&self, pad_index: u8, opus: &[u8]) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(MicFrame {
                pad: pad_index,
                opus: opus.to_vec(),
            });
        }
    }
}

impl Drop for MicPipeline {
    fn drop(&mut self) {
        // Close the channel so the mixer's recv returns Disconnected and the
        // loop exits, then join.
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn mic_loop(device: String, rx: Receiver<MicFrame>) {
    let _rt = RealtimeThread::register_audio();

    let mut render = match RenderPlayback::open(&device) {
        Ok(Some(r)) => r,
        Ok(None) => {
            // No matching endpoint: the pad mic simply has nowhere to go.
            return;
        }
        Err(e) => {
            eprintln!("mic: could not open render endpoint '{device}': {e}");
            return;
        }
    };

    // Lazily-created per-pad decoders, and the accumulators that regroup each
    // pad's decoded stereo into fixed 5 ms mix frames.
    let mut decoders: [Option<OpusDecoder>; MAX_PADS_USIZE] = Default::default();
    let mut accs: Vec<FrameAccumulator> = (0..MAX_PADS_USIZE)
        .map(|_| FrameAccumulator::new(MIX_SAMPLES_PER_CH, 2))
        .collect();
    let mut scratch = vec![0i16; MAX_FRAME_SAMPLES * 2];
    let mut tmp = vec![0i16; MIX_FRAME_LEN];
    let mut mix = vec![0i16; MIX_FRAME_LEN];

    loop {
        // Block briefly for the next frame, so we also notice the channel close.
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(frame) => decode_into(frame, &mut decoders, &mut accs, &mut scratch),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        // Absorb any frames that queued while we were busy.
        while let Ok(frame) = rx.try_recv() {
            decode_into(frame, &mut decoders, &mut accs, &mut scratch);
        }
        render_mixed(&mut render, &mut accs, &mut tmp, &mut mix);
    }
}

/// Decode one frame into its pad's accumulator.
fn decode_into(
    frame: MicFrame,
    decoders: &mut [Option<OpusDecoder>; MAX_PADS_USIZE],
    accs: &mut [FrameAccumulator],
    scratch: &mut [i16],
) {
    let pad = frame.pad as usize;
    if pad >= MAX_PADS_USIZE {
        return;
    }
    if decoders[pad].is_none() {
        // Decode every pad to stereo: a mono headset decodes to duplicated
        // channels, matching the stereo the mixer and render sink expect.
        match OpusDecoder::new(48_000, 2) {
            Ok(d) => decoders[pad] = Some(d),
            Err(e) => {
                eprintln!("mic: Opus decoder init failed for pad {pad}: {e}");
                return;
            }
        }
    }
    let dec = decoders[pad].as_mut().expect("just created");
    if let Ok(samples) = dec.decode(&frame.opus, scratch, false) {
        accs[pad].push(&scratch[..samples * 2]);
    }
}

/// Mix and render every whole 5 ms frame available across the pads, summing with
/// saturation so two headsets are both heard. Stops when no pad has a whole
/// frame, or when the endpoint's shared buffer is full (drops the overflow — a
/// mic drops rather than blocking).
fn render_mixed(
    render: &mut RenderPlayback,
    accs: &mut [FrameAccumulator],
    tmp: &mut [i16],
    mix: &mut [i16],
) {
    loop {
        if !accs.iter().any(|a| a.buffered() >= MIX_FRAME_LEN) {
            break;
        }
        for s in mix.iter_mut() {
            *s = 0;
        }
        for a in accs.iter_mut() {
            if a.buffered() >= MIX_FRAME_LEN && a.pop_frame(tmp) {
                for (m, t) in mix.iter_mut().zip(tmp.iter()) {
                    *m = m.saturating_add(*t);
                }
            }
        }
        match render.write_stereo(mix) {
            Ok(0) => break, // endpoint buffer full; render the rest next wakeup
            Ok(_) => {}
            Err(e) => {
                eprintln!("mic: render write failed: {e}");
                break;
            }
        }
    }
}

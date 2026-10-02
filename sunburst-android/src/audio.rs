// SPDX-License-Identifier: GPL-2.0-or-later
#![cfg(target_os = "android")]

//! Opus decode and low-latency AAudio playback.
//!
//! The client thread puts each audio packet back in order ([`crate::plc`]),
//! decodes it with [`OpusDecoder`] and pushes the PCM into a lock-free
//! [`crate::audio_ring`]; AAudio's real-time data callback pops it. The stream is
//! opened `LowLatency` at 48 kHz stereo i16, with AAudio's own buffer cut to two
//! bursts: the ring is the jitter buffer.
//!
//! The ring plays only once it holds a cushion ([`crate::playout`]): a burst
//! plus the peak gap between packet arrivals (decaying slowly) plus a frame,
//! refilled after an underrun, which also raises the peak.
//!
//! A/V sync, without a resampler: audio and video are stamped in the same server
//! clock domain, and both play out on the client's `CLOCK_MONOTONIC` timeline.
//! Residual crystal drift between the server's capture clock and the client's
//! AAudio clock is kept bounded rather than corrected sample-accurately — the
//! producer drops the newest frame once the ring passes the cushion plus another
//! peak gap and a few frames (client running slow), and an underrun rebuffers (client running
//! fast). Both bound the audio buffer, so the A/V offset cannot grow without
//! limit over a long stream.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::Instant;

use ndk::audio::{
    AudioCallbackResult, AudioDirection, AudioFormat, AudioPerformanceMode, AudioSharingMode,
    AudioStream, AudioStreamBuilder,
};
use sunburst_audio::codec::OpusDecoder;
use sunburst_core::instr::{self, Stage};
use sunburst_core::proto::Seq16;
use sunburst_gip_bridge::Bridge;

use crate::audio_ring::{PcmProducer, pcm_ring};
use crate::headset::HeadsetGate;
use crate::mic::MicFramer;
use crate::playout::{Cushion, Gate};
use crate::plc::{AudioReorder, Event};

/// Largest Opus frame we might decode (120 ms at 48 kHz), per channel — the
/// scratch is sized for it even though the server sends 5 ms frames.
const MAX_FRAME_SAMPLES: usize = 5760;

/// Low-latency AAudio playback fed by Opus decode.
pub struct AudioPlayer {
    // Dropped first: closing the stream stops the callback (and drops the
    // consumer it holds) before the producer goes away.
    _stream: AudioStream,
    producer: PcmProducer,
    decoder: OpusDecoder,
    scratch: Vec<i16>,
    channels: usize,
    /// Samples per channel in one packet — the length of a concealed frame.
    frame_samples: usize,
    /// Puts swapped packets back in order, and says which lost ones to conceal.
    reorder: AudioReorder,
    /// The arrival jitter, and the cushion and trim sized from it. Past the
    /// trim a decoded frame is dropped, to keep buffering (and so A/V offset)
    /// bounded when the client runs slow.
    jitter: Cushion,
    /// The cushion the callback waits for, published from `jitter`.
    target: Arc<AtomicUsize>,
    /// Underruns the callback has had, and how many `jitter` has taken in.
    underruns: Arc<AtomicU32>,
    underruns_seen: u32,
    /// When the previous packet arrived, for the gap to this one.
    last_arrival: Option<Instant>,
    /// Interleaved samples of playback per second, to turn a gap into samples.
    samples_per_sec: f64,
    /// Whether decoded audio plays on the TV (the AAudio stream). Off when the
    /// user routed audio to the pad headset only; the decode still happens so the
    /// pad fork gets its PCM.
    route_tv: bool,
}

impl AudioPlayer {
    /// Open playback for the session's audio parameters and start the stream.
    /// `route_tv` false keeps the stream open but stops feeding it, so audio goes
    /// only to a pad headset.
    pub fn new(
        sample_rate: u32,
        channels: u8,
        frame_samples: u16,
        route_tv: bool,
    ) -> Result<AudioPlayer, String> {
        let ch = channels as usize;
        let frame = frame_samples as usize * ch;
        // Room for the largest cushion and trim (two peaks of 24 frames, plus a
        // burst and slack), whatever the burst turns out to be; a power of two
        // after rounding.
        let capacity = frame * 64 + 4096;

        let (producer, mut consumer) = pcm_ring(capacity);
        // Never reached until the stream is open and the real target published.
        let target = Arc::new(AtomicUsize::new(usize::MAX));
        let callback_target = Arc::clone(&target);
        let underruns = Arc::new(AtomicU32::new(0));
        let callback_underruns = Arc::clone(&underruns);
        let mut gate = Gate::default();

        let stream = AudioStreamBuilder::new()
            .map_err(|e| format!("AAudio unavailable: {e}"))?
            .sample_rate(sample_rate as i32)
            .channel_count(channels as i32)
            .format(AudioFormat::PCM_I16)
            .performance_mode(AudioPerformanceMode::LowLatency)
            .sharing_mode(AudioSharingMode::Shared)
            .direction(AudioDirection::Output)
            .data_callback(Box::new(move |_stream, buf: *mut c_void, frames: i32| {
                // SAFETY: AAudio provides `frames * channel_count` writable i16
                // samples at `buf` for the stream's PCM_I16 format.
                let out = unsafe {
                    std::slice::from_raw_parts_mut(buf as *mut i16, frames as usize * ch)
                };
                let mut got = 0;
                let target = callback_target.load(Ordering::Relaxed);
                if gate.should_play(consumer.available(), target) {
                    got = consumer.pop(out);
                    if got < out.len() {
                        gate.underrun();
                        callback_underruns.fetch_add(1, Ordering::Relaxed);
                    }
                }
                for slot in &mut out[got..] {
                    *slot = 0; // underrun: play silence rather than stale data
                }
                AudioCallbackResult::Continue
            }))
            .open_stream()
            .map_err(|e| format!("AAudio open failed: {e}"))?;

        // AAudio's own buffer defaults far deeper than the ring needs (72 ms
        // was seen queued in it on the Homatics). The ring is the jitter
        // buffer; AAudio only needs enough to ride out a late callback.
        let burst = stream.frames_per_burst();
        let _ = stream.set_buffer_size_in_frames(burst * 2);
        log::info!(
            "audio: burst {burst} frames, AAudio buffer {} of {} frames",
            stream.buffer_size_in_frames(),
            stream.buffer_capacity_in_frames()
        );

        // The callback's size is what the cushion must cover beyond the jitter.
        let jitter = Cushion::new(burst.max(1) as usize * ch, frame);
        target.store(jitter.target(), Ordering::Relaxed);

        stream
            .request_start()
            .map_err(|e| format!("AAudio start failed: {e}"))?;

        let decoder =
            OpusDecoder::new(sample_rate, channels).map_err(|e| format!("Opus decoder: {e}"))?;

        Ok(AudioPlayer {
            _stream: stream,
            producer,
            decoder,
            scratch: vec![0i16; MAX_FRAME_SAMPLES * ch],
            channels: ch,
            frame_samples: frame_samples as usize,
            reorder: AudioReorder::default(),
            jitter,
            target,
            underruns,
            underruns_seen: 0,
            last_arrival: None,
            samples_per_sec: f64::from(sample_rate) * ch as f64,
            route_tv,
        })
    }

    /// Take one audio packet (sequence number `id`): put it back in order,
    /// conceal what is lost (Opus PLC, see [`crate::plc`]), decode, and play each
    /// result on the TV unless routed away or the ring is over its trim. `fork`
    /// gets every played chunk of interleaved stereo PCM, for a pad headset.
    pub fn feed(&mut self, payload: &[u8], id: u32, mut fork: impl FnMut(&[i16])) {
        let AudioPlayer {
            producer,
            decoder,
            scratch,
            channels,
            frame_samples,
            reorder,
            jitter,
            target,
            underruns,
            underruns_seen,
            last_arrival,
            samples_per_sec,
            route_tv,
            ..
        } = self;
        // Size the cushion from this packet's gap and any underruns since.
        let now = Instant::now();
        if let Some(prev) = last_arrival.replace(now) {
            jitter.on_arrival(((now - prev).as_secs_f64() * *samples_per_sec) as usize);
        }
        let seen = underruns.load(Ordering::Relaxed);
        for _ in 0..seen.wrapping_sub(*underruns_seen) {
            jitter.on_underrun();
        }
        *underruns_seen = seen;
        target.store(jitter.target(), Ordering::Relaxed);
        let trim = jitter.trim_above();
        reorder.push(Seq16(id as u16), payload, |event| {
            let (samples, played) = match event {
                Event::Decode(seq, opus) => {
                    let Ok(n) = decoder.decode(opus, scratch, false) else {
                        return;
                    };
                    instr::record(Stage::AudioDecode, seq.0 as u32);
                    (n, seq.0 as u32)
                }
                Event::Conceal => match decoder.conceal(scratch, *frame_samples) {
                    Ok(n) => (n, id),
                    Err(_) => return,
                },
            };
            let pcm = &scratch[..samples * *channels];
            if *route_tv && producer.available() <= trim {
                producer.push(pcm);
                instr::record(Stage::AudioPlay, played);
            }
            fork(pcm);
        });
    }
}

/// The Xbox pads' headsets — both directions behind one shared ≤2-headset cap.
///
/// Replaces the earlier split `PadAudio` (playback fork) and `MicCapture` (mic):
/// an Xbox headset's audio sub-device is full-duplex over one shared iso-bandwidth
/// slot, so enabling it for playback and for the mic must share a single
/// [`HeadsetGate`] — otherwise the two could enable four headsets between them.
/// Sharing the gate is also what lets the mic work on a **TV-only** audio route:
/// [`Self::capture`] enables a headset itself (playback no longer has to), so the
/// mic is live even when server audio is not being forked to the pad.
///
/// The driver takes interleaved 48 kHz stereo for playback and hands back the mic's
/// native capture format (see [`crate::mic::MicFramer`]). A headset appears only
/// after the GIP security handshake, so both methods poll the bridge each call to
/// pick one up and to drop one that went away.
pub struct PadHeadsets {
    gate: HeadsetGate,
    volume: u8,
    /// One encoder pipeline per pad's mic, created when it first produces samples
    /// and rebuilt if the capture format changes.
    framers: [Option<MicFramer>; MAX_PADS],
    /// Scratch for one bridge mic read (a large Opus frame's worth of stereo).
    read: Vec<i16>,
}

const MAX_PADS: usize = sunburst_core::proto::input::MAX_PADS as usize;

/// Fixed mic bitrate: voice at ≤48 kHz needs little, and it stays off the video
/// path, so there is no config knob for it.
const MIC_BITRATE_KBPS: u32 = 32;

impl PadHeadsets {
    pub fn new(volume: u8) -> PadHeadsets {
        PadHeadsets {
            gate: HeadsetGate::new(),
            volume,
            framers: Default::default(),
            read: vec![0i16; MAX_FRAME_SAMPLES * 2],
        }
    }

    /// Fork one interleaved-stereo frame of decoded server audio to every headset
    /// pad's headphones. Called only when the audio route includes the pad.
    pub fn play(&mut self, pcm: &[i16]) {
        let Some(bridge) = crate::usb::current_bridge() else {
            return;
        };
        for pad in 0..MAX_PADS {
            if Self::gone(&bridge, pad) {
                self.forget(pad);
                continue;
            }
            // Only a headset with a speaker, and only once its sub-device is enabled.
            if bridge.audio_format(pad as u8).is_some() && self.ensure_enabled(&bridge, pad) {
                bridge.audio_out(pad as u8, pcm);
            }
        }
    }

    /// Poll every headset pad's microphone and `send` each encoded frame as
    /// `(pad_index, sequence, Opus packet)`. Enables the headset itself, so the mic
    /// works on any audio route — including TV-only, where `play` is never called.
    /// Non-blocking; does nothing without a bridge or when no mic samples are ready.
    pub fn capture(&mut self, mut send: impl FnMut(u8, Seq16, &[u8])) {
        let Some(bridge) = crate::usb::current_bridge() else {
            return;
        };
        for pad in 0..MAX_PADS {
            if Self::gone(&bridge, pad) {
                self.forget(pad);
                continue;
            }
            // The mic's *capture* format (often 24 kHz mono), not the render format;
            // `None` means this headset has no microphone.
            let Some(mf) = bridge.mic_format(pad as u8) else {
                continue;
            };
            if !self.ensure_enabled(&bridge, pad) {
                continue; // over the shared ≤2 cap
            }
            let n = bridge.audio_in(pad as u8, &mut self.read);
            if n == 0 {
                continue;
            }
            let channels = mf.channels as usize;
            // Create (or rebuild on a format change) the pad's framer.
            if self.framers[pad].as_ref().map(MicFramer::format) != Some((mf.rate, channels)) {
                match MicFramer::new(mf.rate, channels, MIC_BITRATE_KBPS) {
                    Ok(f) => self.framers[pad] = Some(f),
                    Err(e) => {
                        log::warn!("mic: pad {pad} framer init failed: {e}");
                        continue;
                    }
                }
            }
            let framer = self.framers[pad].as_mut().expect("just created");
            framer.push_and_drain(&self.read[..n], |seq, opus| send(pad as u8, seq, opus));
        }
    }

    /// Enable `pad`'s headset sub-device if it is not already, honouring the shared
    /// cap; sets the volume once on a fresh enable. Returns whether it is enabled.
    fn ensure_enabled(&mut self, bridge: &Bridge, pad: usize) -> bool {
        if self.gate.is_enabled(pad) {
            return true;
        }
        if !self.gate.reserve(pad) {
            return false; // ≤2 cap full
        }
        if !bridge.set_audio_enabled(pad as u8, true) {
            self.gate.forget(pad); // the enable did not take; release the slot
            return false;
        }
        bridge.set_audio_volume(pad as u8, self.volume);
        true
    }

    /// A headset went away: release its cap slot and drop its mic encoder (so a
    /// re-inserted headset starts its sequence afresh).
    fn forget(&mut self, pad: usize) {
        self.gate.forget(pad);
        self.framers[pad] = None;
    }

    /// Whether pad `pad` has no headset audio sub-device at all (neither speaker nor
    /// mic) — the signal to release it.
    fn gone(bridge: &Bridge, pad: usize) -> bool {
        bridge.audio_format(pad as u8).is_none() && bridge.mic_format(pad as u8).is_none()
    }
}

// SPDX-License-Identifier: GPL-2.0-or-later
#![cfg(target_os = "android")]

//! The client frame path: connect, negotiate, and run receive → decode →
//! present. Mirrors `tools/fakeclient/src/stream.rs`, but feeds the reassembled
//! frames to a real [`Decoder`] on the `SurfaceView` instead of a file.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use ndk::native_window::NativeWindow;
use sunburst_core::instr::{self, Stage};
use sunburst_core::proto::{
    Feedback, Hello, InputPacket, Seq16, ServerControl, StreamCodec, pairing::NONCE_LEN,
};
use sunburst_net::{
    Accept, ClientEndpoint, Inbound, JitterBuffer, OwdGradient, Reassembler, TickUnwrap,
};

use crate::decode::Decoder;
use crate::input_map::{ClientInput, InputAccumulator};

/// CLOCK_MONOTONIC nanoseconds — the base `MediaCodec` release timestamps and
/// `System.nanoTime()` share.
// The `as i64` casts are redundant on arm64 (64-bit fields) but needed on
// armv7, where `timespec`'s fields are narrower; keep them for both ABIs.
#[allow(clippy::unnecessary_cast)]
fn mono_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable out-param for clock_gettime.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}

/// Run the client until `stop` is set. `codecs` is the bitmask the device can
/// decode (`sunburst_core::proto::codecs`); the server negotiates one of them.
pub fn run(
    server: SocketAddr,
    secret: [u8; 32],
    codecs: u8,
    window: NativeWindow,
    stop: Arc<AtomicBool>,
    input_rx: Receiver<ClientInput>,
    client_tid: Arc<AtomicI32>,
) {
    // Publish our tid so the Java PerformanceHintManager can target this thread.
    // SAFETY: gettid takes no arguments and cannot fail.
    let tid = unsafe { libc::gettid() };
    client_tid.store(tid, Ordering::Relaxed);
    if let Err(e) = run_inner(server, secret, codecs, &window, &stop, &input_rx) {
        log::error!("client stopped: {e}");
    }
}

fn run_inner(
    server: SocketAddr,
    secret: [u8; 32],
    codecs: u8,
    window: &NativeWindow,
    stop: &AtomicBool,
    input_rx: &Receiver<ClientInput>,
) -> Result<(), String> {
    let mut client = ClientEndpoint::connect_paired(server, secret).map_err(|e| e.to_string())?;

    let mut client_nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut client_nonce).map_err(|e| e.to_string())?;
    client
        .send_hello(Hello {
            client_id: 1,
            name: "sunburst-android".into(),
            abi: std::env::consts::ARCH.into(),
            width: 3840,
            height: 2160,
            refresh_mhz: 60_000,
            client_nonce,
            clock_offset_ns: 0,
            codecs,
        })
        .map_err(|e| e.to_string())?;

    // Await the negotiation: SessionConfig then CodecPrivate.
    let mut config = None;
    let mut csd: Option<(StreamCodec, Vec<u8>)> = None;
    let deadline = Instant::now() + Duration::from_secs(5);
    while (config.is_none() || csd.is_none()) && !stop.load(Ordering::Relaxed) {
        if Instant::now() >= deadline {
            return Err("negotiation timed out; is the server streaming?".into());
        }
        match client.recv().map_err(|e| e.to_string())? {
            Some(Inbound::Control(ServerControl::SessionConfig(c))) => {
                log::info!(
                    "session {:?} {}x{} @ {} kbps",
                    c.codec,
                    c.width,
                    c.height,
                    c.bitrate_kbps
                );
                config = Some(c);
            }
            Some(Inbound::Control(ServerControl::CodecPrivate { codec, data })) => {
                csd = Some((codec, data));
            }
            Some(_) => {}
            None => client.tick().map_err(|e| e.to_string())?,
        }
    }
    let config = config.ok_or("no SessionConfig")?;
    let (_, csd0) = csd.ok_or("no CodecPrivate")?;
    let fps = (config.fps_mhz.max(1000) / 1000) as i32;

    let decoder = Decoder::new(
        config.codec,
        config.width as i32,
        config.height as i32,
        fps,
        &csd0,
        config.hdr,
        window,
    )?;

    let mut reassembler = Reassembler::new();
    let mut jitter = JitterBuffer::new();
    let frame_interval_ns = 1_000_000_000u64 / fps.max(1) as u64;
    jitter.set_frame_interval_ns(frame_interval_ns);
    jitter.set_min_depth_ns(2_000_000);
    let mut owd = OwdGradient::new(100_000_000);
    let mut ticks = TickUnwrap::new(config.qpc_freq_hz);

    let mut out = vec![0u8; 8 * 1024 * 1024];
    let mut targets = [0u16; 128];
    let mut abandoned = [Seq16(0); 16];
    let mut pts_us = 0u64;
    let frame_interval_us = 1_000_000u64 / fps.max(1) as u64;
    let mut newest_seen: Option<Seq16> = None;
    let mut received = 0u32;
    let mut dropped = 0u32;
    let mut last_feedback = Instant::now();
    let mut input_acc = InputAccumulator::new();
    let mut input_seq: u32 = 1;

    while !stop.load(Ordering::Relaxed) {
        match client.recv().map_err(|e| e.to_string())? {
            Some(Inbound::Video(pkt)) => {
                let fid = sunburst_core::proto::Header::decode(&pkt).map(|h| h.frame_id);
                if let Some(fid) = fid {
                    newest_seen = Some(match newest_seen {
                        Some(n) if n.is_newer_than(fid) => n,
                        _ => fid,
                    });
                }
                match reassembler.push(&pkt) {
                    Accept::Complete(frame) => {
                        received += 1;
                        instr::record(Stage::Recv, frame.frame_id.0 as u32);
                        owd.push(ticks.to_ns(frame.qpc_timestamp), mono_ns());
                        if let Err(f) = jitter.push(frame, mono_ns() as u64) {
                            reassembler.release(f);
                        }
                    }
                    Accept::Buffered => {
                        if let Some(fid) = fid {
                            let newer = newest_seen.is_some_and(|n| n.is_newer_than(fid));
                            if newer || reassembler.expected_count(fid).is_some() {
                                let n = reassembler.nack_targets(fid, newer, &mut targets);
                                if n > 0 {
                                    let _ = client.send_nack(fid, &targets[..n]);
                                }
                            }
                        }
                    }
                    Accept::Ignored => {}
                }
            }
            Some(_) => {}
            None => client.tick().map_err(|e| e.to_string())?,
        }

        // Release due frames into the decoder.
        while let Some(rel) = jitter.pop(mono_ns() as u64) {
            instr::record(Stage::JitterOut, rel.frame.frame_id.0 as u32);
            if let Some(from) = rel.stepped_over {
                dropped += 1;
                let _ = client.send_nack(from, &[]);
                let mut id = from;
                while id != rel.frame.frame_id {
                    reassembler.discard(id);
                    id = id.next();
                }
            }
            let fid = rel.frame.frame_id.0 as u32;
            if let Some(n) = reassembler.copy_into(&rel.frame, &mut out) {
                match decoder.feed(&out[..n], pts_us, fid) {
                    Ok(true) => pts_us += frame_interval_us,
                    Ok(false) => log::warn!("decoder input full; dropped frame {fid}"),
                    Err(e) => log::error!("feed: {e}"),
                }
            }
            reassembler.release(rel.frame);
        }

        // Present whatever the decoder has finished, at the next vsync.
        if let Err(e) = decoder.drain(mono_ns(), 0) {
            log::error!("drain: {e}");
        }

        let a = reassembler.drain_abandoned(&mut abandoned);
        for id in &abandoned[..a] {
            let _ = client.send_nack(*id, &[]);
        }

        // Drain queued input and send it, mapped to wire events. The session
        // key is installed, so the sequence starts at 1 each session.
        while let Ok(raw) = input_rx.try_recv() {
            if let Some(event) = input_acc.apply(raw)
                && client.send_input(&InputPacket { input_seq, event }).is_ok()
            {
                input_seq = input_seq.wrapping_add(1);
            }
        }

        if last_feedback.elapsed() >= Duration::from_millis(100) {
            last_feedback = Instant::now();
            let _ = client.send_feedback(&Feedback {
                recv_timestamp: mono_ns() as u32,
                frames_received: received,
                frames_dropped: dropped,
                jitter_buffer_ms: (jitter.target_depth_ns() / 1_000_000) as u16,
                decode_p99_us: 0,
                owd_gradient: owd.slope_us_per_s(),
            });
        }
        client.tick().map_err(|e| e.to_string())?;
    }

    decoder.stop();
    Ok(())
}

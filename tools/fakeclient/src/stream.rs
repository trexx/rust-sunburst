// SPDX-License-Identifier: GPL-2.0-or-later

//! The stub video receiver: pair, negotiate, receive, recover, and dump.
//!
//! This is Phase 4's acceptance harness and Phase 3's play-on-TV artifact in one
//! command. It drives the real receive path — [`Reassembler`], [`JitterBuffer`],
//! NACK, the delay-gradient estimator, [`Feedback`] — against the server over the
//! wire, writes the decoded elementary stream to disk, and reports the per-stage
//! timings the instrumentation ring collected. No decoder: it proves the
//! transport, not the picture.

use std::io::Write;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use sunburst_core::instr::{self, Collector, Stage};
use sunburst_core::proto::{
    ClientControl, Feedback, Hello, Seq16, ServerControl, StreamCodec, pairing::NONCE_LEN,
};
use sunburst_net::{
    Accept, ClientEndpoint, Inbound, JitterBuffer, OwdGradient, Reassembler, TickUnwrap,
    client_interval_ns,
};

use crate::ivf::IvfWriter;

/// One line on how evenly the delivered frames sample motion: the content
/// (present-time) step between consecutive frames, and how many steps are off
/// the client's own interval by more than 30 %. With content faster than the
/// client, every step should sit near the interval — a 120 Hz desktop into a
/// 60 Hz client alternating 25 / 8 ms is visible judder. (Slower content steps
/// by its own interval, which reads as "off" here; judge those by the median.)
fn describe_steps(steps_ms: &[f64], interval_ms: f64) -> Option<String> {
    if steps_ms.len() < 10 {
        return None;
    }
    let mut s = steps_ms.to_vec();
    s.sort_by(f64::total_cmp);
    let pct = |p: f64| s[((s.len() - 1) as f64 * p) as usize];
    let off = steps_ms
        .iter()
        .filter(|&&d| (d - interval_ms).abs() > 0.3 * interval_ms)
        .count();
    Some(format!(
        "content steps: p5 {:.1} / p50 {:.1} / p95 {:.1} ms; {off} of {} ({:.1}%) off the {:.1} ms client step by >30%",
        pct(0.05),
        pct(0.50),
        pct(0.95),
        s.len(),
        100.0 * off as f64 / s.len() as f64,
        interval_ms,
    ))
}

/// One line on the `av1C` record the server sent, and whether it agrees with the
/// sequence-header OBU it wraps. A record that disagrees is CLAUDE.md's silent
/// failure — the decoder configures and then outputs nothing — so this is
/// checked on every AV1 run rather than trusted.
fn describe_av1c(record: &[u8]) -> String {
    use sunburst_core::codec::av1::parse_sequence_header;
    if record.len() < 4 || record[0] != 0x81 {
        return format!("av1C: malformed ({} bytes)", record.len());
    }
    let (profile, level, tier) = (record[1] >> 5, record[1] & 0x1f, record[2] >> 7);
    let verdict = match parse_sequence_header(&record[4..]) {
        Some(seq)
            if (seq.seq_profile, seq.seq_level_idx, seq.seq_tier) == (profile, level, tier) =>
        {
            "matches its sequence header".to_string()
        }
        Some(seq) => format!(
            "MISMATCH: the sequence header says profile {} level {} tier {}",
            seq.seq_profile, seq.seq_level_idx, seq.seq_tier
        ),
        None => "no parsable sequence header inside".to_string(),
    };
    format!("av1C: profile {profile} level {level} tier {tier} — {verdict}")
}

pub struct StreamOpts {
    pub codecs: u8,
    /// A codec the client *requests* (tests the server honouring it); `None` = none.
    pub prefer_codec: Option<sunburst_core::proto::StreamCodec>,
    /// A client bitrate ceiling in kbps; `0` = none.
    pub max_bitrate_kbps: u32,
    pub out: Option<String>,
    pub drop_pct: u8,
    pub retransmit: bool,
    pub secs: u64,
    pub stats: bool,
}

/// A tiny deterministic RNG for the loss shim — no dependency, and reproducible
/// so a flaky run is the network's fault, not the tool's.
struct Lcg(u64);
impl Lcg {
    fn next_pct(&mut self) -> u8 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((self.0 >> 33) % 100) as u8
    }
}

enum Sink {
    Annexb(std::io::BufWriter<std::fs::File>),
    Ivf(IvfWriter),
    None,
}

pub fn stream(server: SocketAddr, secret: [u8; 32], opts: StreamOpts) -> Result<(), String> {
    let mut client = ClientEndpoint::connect_paired(server, secret).map_err(|e| e.to_string())?;

    let mut client_nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut client_nonce).map_err(|e| e.to_string())?;
    client
        .send_hello(Hello {
            client_id: 1,
            name: "fakeclient".into(),
            abi: std::env::consts::ARCH.into(),
            width: 3840,
            height: 2160,
            refresh_mhz: 60_000,
            client_nonce,
            clock_offset_ns: 0,
            codecs: opts.codecs,
            prefer_codec: opts.prefer_codec,
            max_bitrate_kbps: opts.max_bitrate_kbps,
        })
        .map_err(|e| e.to_string())?;

    // Await the session negotiation.
    let mut config = None;
    let mut headers: Option<(StreamCodec, Vec<u8>)> = None;
    let deadline = Instant::now() + Duration::from_secs(5);
    while (config.is_none() || headers.is_none()) && Instant::now() < deadline {
        match client.recv().map_err(|e| e.to_string())? {
            Some(Inbound::Control(ServerControl::SessionConfig(c))) => {
                println!(
                    "session: {:?} {}x{} @ {} kbps{}",
                    c.codec,
                    c.width,
                    c.height,
                    c.bitrate_kbps,
                    if c.hdr.is_some() { " HDR" } else { "" }
                );
                config = Some(c);
            }
            Some(Inbound::Control(ServerControl::CodecPrivate { codec, data })) => {
                if codec == StreamCodec::Av1 {
                    println!("{}", describe_av1c(&data));
                }
                headers = Some((codec, data));
            }
            Some(_) => {}
            None => client.tick().map_err(|e| e.to_string())?,
        }
    }
    let config = config.ok_or("no SessionConfig arrived; is the server streaming?")?;

    // Ask for an IDR now that we're consuming: the server fired its startup IDR
    // the instant it spawned the pipeline, before this negotiation finished, so
    // the reassembler missed it. A real client requests one the same way.
    client
        .send_control(&ClientControl::RequestIdr)
        .map_err(|e| e.to_string())?;

    // The decoder-config bytes lead the dump so it stands alone; the forced-IDR
    // also inlines them, but a leading copy makes a raw file playable from byte 0.
    let mut sink = match &opts.out {
        Some(path) => {
            if config.codec == StreamCodec::Av1 {
                Sink::Ivf(
                    IvfWriter::create(path, config.width, config.height, config.fps_mhz / 1000)
                        .map_err(|e| e.to_string())?,
                )
            } else {
                let mut w = std::io::BufWriter::new(
                    std::fs::File::create(path).map_err(|e| e.to_string())?,
                );
                if let Some((_, data)) = &headers {
                    w.write_all(data).map_err(|e| e.to_string())?;
                }
                Sink::Annexb(w)
            }
        }
        None => Sink::None,
    };

    instr::register_thread("recv");
    let mut collector = Collector::new();
    let mut reassembler = Reassembler::new();
    let mut jitter = JitterBuffer::new();
    // Exact: truncating 59.94 Hz to whole frames would govern the jitter buffer
    // (and the content-step check below) at 59.
    let frame_interval_ns = client_interval_ns(config.fps_mhz, 0);
    jitter.set_frame_interval_ns(frame_interval_ns);
    jitter.set_min_depth_ns(2_000_000); // hold ~2 ms so a retransmit can land
    let mut owd = OwdGradient::new(100_000_000);
    let mut ticks = TickUnwrap::new(config.qpc_freq_hz);
    // Content steps: the present-time advance between consecutive delivered
    // frames. How evenly the server sampled motion — judder no client can
    // smooth — is visible here directly.
    let mut content_ticks = TickUnwrap::new(config.qpc_freq_hz);
    let mut last_content: Option<i64> = None;
    let mut steps_ms: Vec<f64> = Vec::new();

    let mut rng = Lcg(0x1234_5678_9abc_def0);
    let mut out_buf = vec![0u8; 8 * 1024 * 1024];
    let mut targets = [0u16; 128];
    let mut abandoned = [Seq16(0); 16];

    let origin = Instant::now();
    let end = origin + Duration::from_secs(opts.secs);
    let mut last_feedback = origin;
    let mut newest_seen: Option<Seq16> = None;
    let now_ns = |t: Instant| t.duration_since(origin).as_nanos() as u64;

    // Counters for the summary.
    let (mut delivered, mut keyframes, mut stepped, mut nacks, mut abandons, mut received) =
        (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    // Audio is server→client like video; fakeclient does not decode Opus, but it
    // counts audio packets so the server's audio send path is provable off-box.
    let mut audio_pkts = 0u64;

    while Instant::now() < end {
        match client.recv().map_err(|e| e.to_string())? {
            Some(Inbound::Video(pkt)) => {
                if opts.drop_pct > 0 && rng.next_pct() < opts.drop_pct {
                    continue; // simulated loss
                }
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
                        owd.push(
                            ticks.to_ns(frame.qpc_timestamp),
                            now_ns(Instant::now()) as i64,
                        );
                        if let Err(f) = jitter.push(frame, now_ns(Instant::now())) {
                            reassembler.release(f);
                        }
                    }
                    Accept::Buffered => {
                        // NACK this frame's holes if a newer one has begun (so a
                        // lost terminator is asked for too), and the frame behind.
                        if let Some(fid) = fid {
                            let newer = newest_seen.is_some_and(|n| n.is_newer_than(fid));
                            nacks += send_nack_for(
                                &mut client,
                                &reassembler,
                                fid,
                                newer,
                                &mut targets,
                                opts.retransmit,
                            );
                            let prev = Seq16(fid.0.wrapping_sub(1));
                            if reassembler.is_pending(prev) {
                                nacks += send_nack_for(
                                    &mut client,
                                    &reassembler,
                                    prev,
                                    true,
                                    &mut targets,
                                    opts.retransmit,
                                );
                            }
                        }
                    }
                    Accept::Ignored => {}
                }
            }
            Some(Inbound::Audio(_)) => audio_pkts += 1,
            // The stub receiver has no pad to drive; count rumble/pad-output with
            // the rest of the ignored control traffic.
            Some(Inbound::Rumble(_)) | Some(Inbound::PadOutput(_)) => {}
            Some(Inbound::Control(_)) | Some(Inbound::Other) => {}
            None => client.tick().map_err(|e| e.to_string())?,
        }

        // Release anything due, dumping it and NACK-abandoning what it skipped.
        let now = now_ns(Instant::now());
        while let Some(rel) = jitter.pop(now) {
            if let Some(from) = rel.stepped_over {
                stepped += 1;
                abandons += 1;
                let _ = client.send_nack(from, &[]);
                let mut id = from;
                while id != rel.frame.frame_id {
                    reassembler.discard(id);
                    id = id.next();
                }
            }
            if rel.frame.keyframe {
                keyframes += 1;
            }
            let content = content_ticks.to_ns(rel.frame.qpc_timestamp);
            if let Some(prev) = last_content {
                let step = content - prev;
                // A still desktop (or a stall) is not a motion step.
                if step > 0 && step < 100_000_000 {
                    steps_ms.push(step as f64 / 1e6);
                }
            }
            last_content = Some(content);
            if let Some(n) = reassembler.copy_into(&rel.frame, &mut out_buf) {
                instr::record(Stage::JitterOut, rel.frame.frame_id.0 as u32);
                match &mut sink {
                    Sink::Annexb(w) => {
                        let _ = w.write_all(&out_buf[..n]);
                    }
                    Sink::Ivf(w) => {
                        let _ = w.write_frame(&out_buf[..n]);
                    }
                    Sink::None => {}
                }
                delivered += 1;
            }
            reassembler.release(rel.frame);
        }

        // Frames the reassembler gave up on (pool pressure) are abandon-NACKed.
        let a = reassembler.drain_abandoned(&mut abandoned);
        for id in &abandoned[..a] {
            abandons += 1;
            let _ = client.send_nack(*id, &[]);
        }

        // Feedback every 100 ms.
        if Instant::now().duration_since(last_feedback) >= Duration::from_millis(100) {
            last_feedback = Instant::now();
            let _ = client.send_feedback(&Feedback {
                recv_timestamp: now as u32,
                frames_received: received as u32,
                frames_dropped: stepped as u32,
                jitter_buffer_ms: (jitter.target_depth_ns() / 1_000_000) as u16,
                decode_p99_us: 0,
                owd_gradient: owd.slope_us_per_s(),
            });
        }
        collector.poll();
        client.tick().map_err(|e| e.to_string())?;
    }

    let frames = match sink {
        Sink::Annexb(mut w) => {
            w.flush().map_err(|e| e.to_string())?;
            None
        }
        Sink::Ivf(w) => Some(w.finish().map_err(|e| e.to_string())?),
        Sink::None => None,
    };

    // End the session now rather than leaving the server to time it out, so
    // the next run is not refused as a second stream.
    client.bye();
    collector.poll();
    collector.rotate();
    println!(
        "received {received}, delivered {delivered} ({keyframes} keyframes), \
         stepped {stepped}, abandons {abandons}, nacks {nacks}, audio {audio_pkts}"
    );
    if let Some(f) = frames {
        println!("wrote {f} IVF frames");
    }
    if let Some(line) = describe_steps(&steps_ms, frame_interval_ns as f64 / 1e6) {
        println!("{line}");
    }
    if opts.stats {
        let report = collector.report();
        for stage in [Stage::Recv, Stage::JitterOut] {
            if let Some(s) = report.stage(stage) {
                println!(
                    "{:>10}: p50 {} us  p99 {} us  n {}",
                    stage.name(),
                    s.p50_ns / 1000,
                    s.p99_ns / 1000,
                    s.count
                );
            }
        }
    }
    Ok(())
}

/// NACK a pending frame's missing packets (or abandon), returning 1 if a NACK
/// was sent. Retransmit-off callers still abandon, so recovery is exercised.
fn send_nack_for(
    client: &mut ClientEndpoint,
    reassembler: &Reassembler,
    frame_id: Seq16,
    newer_seen: bool,
    targets: &mut [u16],
    retransmit: bool,
) -> u64 {
    if !retransmit {
        return 0;
    }
    let n = reassembler.nack_targets(frame_id, newer_seen, targets);
    if n > 0 && (newer_seen || reassembler.expected_count(frame_id).is_some()) {
        let _ = client.send_nack(frame_id, &targets[..n]);
        1
    } else {
        0
    }
}

#[cfg(test)]
mod step_tests {
    use super::describe_steps;

    #[test]
    fn even_steps_are_clean_and_25_8_judder_is_counted() {
        let even = vec![16.67; 100];
        assert!(
            describe_steps(&even, 16.67)
                .unwrap()
                .contains("0 of 100 (0.0%)")
        );
        // What the old governor did at 120 Hz -> 60: pairs of 25 / 8 ms steps.
        let judder: Vec<f64> = (0..100)
            .map(|i| {
                if i % 6 == 0 {
                    25.0
                } else if i % 6 == 1 {
                    8.3
                } else {
                    16.67
                }
            })
            .collect();
        let line = describe_steps(&judder, 16.67).unwrap();
        assert!(line.contains("34 of 100"), "{line}");
        assert!(
            describe_steps(&even[..5], 16.67).is_none(),
            "too few to say"
        );
    }
}

#[cfg(test)]
mod av1c_tests {
    use super::describe_av1c;

    /// NVENC's AV1 sequence-header OBU from a real 4K capture: level 13, High tier.
    const SEQ: [u8; 17] = [
        0x0a, 0x0f, 0x00, 0x00, 0x00, 0x6e, 0xef, 0xbf, 0xe1, 0xbc, 0x02, 0x19, 0xd0, 0x91, 0x00,
        0x90, 0x40,
    ];

    fn record(profile: u8, level: u8, tier: u8) -> Vec<u8> {
        let mut r = vec![0x81, (profile << 5) | level, (tier << 7) | 0x4c, 0];
        r.extend_from_slice(&SEQ);
        r
    }

    #[test]
    fn a_record_that_agrees_with_its_obu_passes() {
        assert!(describe_av1c(&record(0, 13, 1)).ends_with("matches its sequence header"));
    }

    #[test]
    fn the_old_main_tier_record_is_caught() {
        // What the server sent before it parsed NVENC's header: tier 0.
        let line = describe_av1c(&record(0, 13, 0));
        assert!(line.contains("MISMATCH"), "{line}");
    }

    #[test]
    fn a_malformed_record_says_so() {
        assert!(describe_av1c(&[0x00, 1, 2]).contains("malformed"));
    }
}

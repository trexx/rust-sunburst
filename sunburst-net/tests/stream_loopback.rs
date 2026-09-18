// SPDX-License-Identifier: GPL-2.0-or-later

//! Phase 4's acceptance criterion, minus the encoder, on the host: a
//! synthetic sender and the real receiver over a real UDP socket with 2 %
//! packet loss.
//!
//! With retransmission every frame completes and nothing is abandoned. With
//! retransmission switched off the receiver steps over the damaged frames,
//! abandons them, and the sender's reference-state machine answers with
//! invalidations rather than keyframes — the recovery path that keeps a
//! hitch off the screen.

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use sunburst_core::proto::{HEADER_LEN, Header, MAX_PAYLOAD, Nack, PacketType, Seq16};
use sunburst_net::{
    Accept, HevcRefState, JitterBuffer, Packetizer, Reassembler, Recovery, RefState,
    RetransmitCache,
};

/// Ends on frame 115, which the loss pattern below leaves intact, so the
/// receiver sees its last frame in both modes rather than waiting out a
/// deadline for one that can never complete.
const FRAMES: u16 = 116;
/// Forty full packets plus the terminator per frame.
const UNIT_BYTES: usize = 40 * MAX_PAYLOAD;
const FRAME_INTERVAL: Duration = Duration::from_millis(8);
const FILL: u8 = 0x33;

/// Deterministic ~2 % loss that also hits a terminator now and then
/// (frame 9's, for one).
fn lost(frame: u16, pkt_idx: u16) -> bool {
    (frame as u32 * 7 + pkt_idx as u32) % 50 == 3
}

fn frames_with_loss() -> usize {
    (0..FRAMES)
        .filter(|f| (0..=40u16).any(|i| lost(*f, i)))
        .count()
}

#[derive(Default, Debug)]
struct SenderReport {
    retransmits: u32,
    abandons: u32,
    invalidated_frames: u32,
    force_idr: u32,
}

fn nack_header(frame_id: Seq16) -> [u8; HEADER_LEN] {
    let mut head = [0u8; HEADER_LEN];
    Header {
        packet_type: PacketType::Nack,
        flags: sunburst_core::proto::Flags::EMPTY,
        frame_id,
        qpc_timestamp: 0,
        pkt_idx: 0,
        pkt_count: 1,
    }
    .encode(&mut head);
    head
}

fn sender(sock: UdpSocket, retransmit: bool) -> SenderReport {
    let mut buf = [0u8; HEADER_LEN + MAX_PAYLOAD];
    let (_, peer) = sock.recv_from(&mut buf).expect("receiver hello");
    sock.set_read_timeout(Some(Duration::from_micros(500)))
        .unwrap();

    let mut packetizer = Packetizer::new();
    let mut cache = RetransmitCache::new(4096);
    let mut refs = HevcRefState::new(8);
    let unit = vec![FILL; UNIT_BYTES];
    let mut report = SenderReport::default();

    let mut service = |cache: &mut RetransmitCache,
                       refs: &mut HevcRefState,
                       report: &mut SenderReport,
                       until: Instant| {
        while Instant::now() < until {
            let Ok((n, _)) = sock.recv_from(&mut buf) else {
                continue;
            };
            let Some(h) = Header::decode(&buf[..n]) else {
                continue;
            };
            if h.packet_type != PacketType::Nack {
                continue;
            }
            let Some(nack) = Nack::decode(&buf[HEADER_LEN..n]) else {
                continue;
            };
            if nack.is_abandon() {
                report.abandons += 1;
                match refs.on_abandoned(h.frame_id) {
                    Recovery::Invalidate { from, to } => {
                        let mut id = from;
                        loop {
                            assert!(refs.timestamp_of(id).is_some(), "untracked {id:?}");
                            report.invalidated_frames += 1;
                            if id == to {
                                break;
                            }
                            id = id.next();
                        }
                    }
                    Recovery::ForceIdr => report.force_idr += 1,
                    Recovery::Nothing => {}
                }
            } else if retransmit {
                for idx in nack.missing() {
                    if let Some(pkt) = cache.get(h.frame_id, idx) {
                        sock.send_to(pkt, peer).unwrap();
                        report.retransmits += 1;
                    }
                }
            }
        }
    };

    for f in 0..FRAMES {
        let fid = Seq16(f);
        packetizer.begin_frame(fid, f as u32 * 166_667, f == 0);
        let mut emit = |pkt: &[u8]| {
            cache.store(pkt);
            let h = Header::decode(pkt).unwrap();
            if !lost(f, h.pkt_idx) {
                sock.send_to(pkt, peer).unwrap();
            }
        };
        packetizer.push_unit(&unit, &mut emit);
        packetizer.finish_frame(&mut emit);
        refs.on_encoded(fid, f as u64, f == 0);
        service(
            &mut cache,
            &mut refs,
            &mut report,
            Instant::now() + FRAME_INTERVAL,
        );
    }
    service(
        &mut cache,
        &mut refs,
        &mut report,
        Instant::now() + Duration::from_millis(300),
    );
    report
}

#[derive(Default, Debug)]
struct ReceiverReport {
    delivered: u32,
    stepped_over: u32,
    abandons_sent: u32,
    nacks_sent: u32,
}

fn receiver(sock: UdpSocket, server: SocketAddr) -> ReceiverReport {
    sock.send_to(b"hi", server).unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(2)))
        .unwrap();

    let mut r = Reassembler::new();
    let mut j = JitterBuffer::new();
    j.set_frame_interval_ns(FRAME_INTERVAL.as_nanos() as u64);
    // A NACK round trip on loopback is well under a millisecond; hold frames
    // that long so a retransmit can land before the next frame is due.
    j.set_min_depth_ns(1_000_000);

    let mut out = vec![0u8; UNIT_BYTES + MAX_PAYLOAD];
    let mut targets = [0u16; 64];
    let mut body = [0u8; 2 + 64 * 2];
    let mut abandoned = [Seq16(0); 16];
    let mut buf = [0u8; HEADER_LEN + MAX_PAYLOAD];
    let mut report = ReceiverReport::default();
    let mut newest_seen: Option<Seq16> = None;

    let start = Instant::now();
    let deadline = start + Duration::from_secs(4);
    let now = || start.elapsed().as_nanos() as u64;

    let send_nack = |frame_id: Seq16, missing: &[u16], body: &mut [u8]| {
        let mut pkt = [0u8; HEADER_LEN + 2 + 64 * 2];
        pkt[..HEADER_LEN].copy_from_slice(&nack_header(frame_id));
        let n = Nack::encode(missing, body).unwrap();
        pkt[HEADER_LEN..HEADER_LEN + n].copy_from_slice(&body[..n]);
        sock.send_to(&pkt[..HEADER_LEN + n], server).unwrap();
    };

    'run: while Instant::now() < deadline {
        if let Ok((n, _)) = sock.recv_from(&mut buf) {
            let pkt = &buf[..n];
            if let Some(h) = Header::decode(pkt)
                && h.packet_type == PacketType::Video
            {
                let fid = h.frame_id;
                newest_seen = Some(match newest_seen {
                    Some(ns) if ns.is_newer_than(fid) => ns,
                    _ => fid,
                });
                match r.push(pkt) {
                    Accept::Complete(f) => {
                        if let Err(f) = j.push(f, now()) {
                            r.release(f);
                        }
                    }
                    Accept::Buffered => {
                        let newer = newest_seen.is_some_and(|ns| ns.is_newer_than(fid));
                        let k = r.nack_targets(fid, newer, &mut targets);
                        if k > 0 && (newer || r.expected_count(fid).is_some()) {
                            send_nack(fid, &targets[..k], &mut body);
                            report.nacks_sent += 1;
                        }
                    }
                    Accept::Ignored => {}
                }
                // The frame before this one may be waiting on a lost tail.
                let prev = Seq16(fid.0.wrapping_sub(1));
                if r.is_pending(prev) {
                    let k = r.nack_targets(prev, true, &mut targets);
                    if k > 0 {
                        send_nack(prev, &targets[..k], &mut body);
                        report.nacks_sent += 1;
                    }
                }
            }
        }

        while let Some(rel) = j.pop(now()) {
            if let Some(from) = rel.stepped_over {
                report.stepped_over += 1;
                send_nack(from, &[], &mut body);
                report.abandons_sent += 1;
                let mut id = from;
                while id != rel.frame.frame_id {
                    r.discard(id);
                    id = id.next();
                }
            }
            let len = r.copy_into(&rel.frame, &mut out).expect("live handle");
            assert_eq!(len, UNIT_BYTES);
            assert!(out[..len].iter().all(|b| *b == FILL), "corrupt frame");
            report.delivered += 1;
            let last = rel.frame.frame_id;
            r.release(rel.frame);
            if last == Seq16(FRAMES - 1) {
                break 'run;
            }
        }

        let a = r.drain_abandoned(&mut abandoned);
        for id in &abandoned[..a] {
            send_nack(*id, &[], &mut body);
            report.abandons_sent += 1;
        }
    }
    report
}

fn run(retransmit: bool) -> (SenderReport, ReceiverReport) {
    let server_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let server = server_sock.local_addr().unwrap();
    let client_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let sender = std::thread::spawn(move || sender(server_sock, retransmit));
    let recv = receiver(client_sock, server);
    (sender.join().unwrap(), recv)
}

#[test]
fn two_percent_loss_is_recovered_by_retransmission_without_a_hitch() {
    let (s, r) = run(true);
    assert_eq!(r.delivered as u16, FRAMES, "{r:?} / {s:?}");
    assert_eq!(r.stepped_over, 0, "a frame was skipped: {r:?} / {s:?}");
    assert_eq!(r.abandons_sent, 0);
    assert_eq!(s.abandons, 0);
    assert!(s.retransmits > 0, "the loss shim sent nothing to recover");
    assert!(r.nacks_sent > 0);
}

#[test]
fn without_retransmission_abandons_invalidate_references_not_keyframes() {
    let (s, r) = run(false);
    let damaged = frames_with_loss() as u32;
    assert!(damaged > 0);
    assert_eq!(
        r.delivered,
        FRAMES as u32 - damaged,
        "every undamaged frame and nothing else: {r:?} / {s:?}"
    );
    assert!(r.abandons_sent > 0, "{r:?}");
    assert!(s.abandons > 0, "{s:?}");
    assert!(s.invalidated_frames > 0, "{s:?}");
    assert_eq!(
        s.force_idr, 0,
        "single-frame gaps fit an 8-deep DPB, so no keyframe: {s:?}"
    );
}

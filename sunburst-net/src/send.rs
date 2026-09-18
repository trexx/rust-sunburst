// SPDX-License-Identifier: GPL-2.0-or-later

//! Paced, batched sending: the deadline pacer and the send abstraction.
//!
//! Two jobs the send thread needs and that split cleanly by testability.
//!
//! [`Pacer`] spreads a frame's packets across part of the frame interval so a
//! burst cannot overrun a switch's buffer, which on a LAN is what turns into a
//! NACK round trip regardless of headroom. It is paced against the **rate**, not
//! the round trip — the rate controller sets the rate, and the pacer holds the
//! sender to it. The clock is an argument, so the schedule is host-tested.
//!
//! [`Sender`] is how a batch of packets reaches the wire. [`PlainSender`] is one
//! `send_to` per packet and is what the client, the tests, and any non-Windows
//! build use. On Windows [`WsaSender`](windows::WsaSender) hands the kernel a
//! contiguous run of equal-size datagrams in one `WSASendMsg` with a
//! `UDP_SEND_MSG_SIZE` control message — the USO offload that cuts the syscall
//! count ~30× at 4K60 packet rates — and falls back to per-packet sending if the
//! option is refused. [`Batch`] groups the ring's packets into those runs.

use std::io;
use std::net::{SocketAddr, UdpSocket};

use sunburst_core::proto::{HEADER_LEN, MAX_PAYLOAD};

/// The most datagrams one USO batch carries. Kept small so a batch is a fraction
/// of a frame and the pacer still gets to interleave; the syscall win is already
/// most of the way there by 16–32.
pub const MAX_BATCH: usize = 32;

/// Bytes a full batch buffer needs.
pub const BATCH_BYTES: usize = MAX_BATCH * (HEADER_LEN + MAX_PAYLOAD);

// ===========================================================================
// Pacer
// ===========================================================================

/// A leaky-bucket pacer: the wire is free again `bytes*8/rate` after each send,
/// with a small burst allowance so an idle sender is not throttled on its first
/// packet.
pub struct Pacer {
    rate_bps: u64,
    /// The clock time at which the link is next free.
    available_ns: u64,
    /// How far the sender may run ahead of the pace after being idle.
    burst_ns: u64,
}

impl Pacer {
    /// `rate_bps` is bits per second; `now_ns` seeds the clock. A burst of
    /// `burst_ns` worth of idle time is forgiven, so a stream that pauses does
    /// not have to pay it back all at once.
    pub fn new(rate_bps: u64, burst_ns: u64, now_ns: u64) -> Pacer {
        Pacer {
            rate_bps: rate_bps.max(1),
            available_ns: now_ns,
            burst_ns,
        }
    }

    /// Change the target rate (the rate controller moved it). The bucket keeps
    /// its current fill, so the new rate takes effect from the next packet.
    pub fn set_rate(&mut self, rate_bps: u64) {
        self.rate_bps = rate_bps.max(1);
    }

    pub fn rate_bps(&self) -> u64 {
        self.rate_bps
    }

    /// Nanoseconds the caller should wait before sending, given the clock now.
    /// Zero means send immediately.
    pub fn wait_ns(&self, now_ns: u64) -> u64 {
        self.available_ns.saturating_sub(now_ns)
    }

    /// Record `bytes` sent at `now_ns`, advancing when the wire is next free.
    pub fn record(&mut self, bytes: usize, now_ns: u64) {
        // Start from whichever is later: when the link was free, or `burst_ns`
        // before now (so idle time past the burst allowance is not banked).
        let floor = now_ns.saturating_sub(self.burst_ns);
        let base = self.available_ns.max(floor);
        let cost_ns = (bytes as u64 * 8 * 1_000_000_000) / self.rate_bps;
        self.available_ns = base + cost_ns;
    }
}

// ===========================================================================
// Batch
// ===========================================================================

/// Groups packets into a USO-shippable run: every datagram the same size except
/// the last, which may be shorter. A packet that breaks the rule (a different
/// size after a short one, or the batch is full) is refused, so the caller
/// flushes and starts the next batch with it.
pub struct Batch {
    buf: Box<[u8; BATCH_BYTES]>,
    len: usize,
    count: usize,
    seg_size: usize,
    /// Set once a shorter-than-`seg_size` packet is added: it can only be the
    /// final segment, so nothing may follow it.
    closed: bool,
}

impl Default for Batch {
    fn default() -> Self {
        Self::new()
    }
}

impl Batch {
    pub fn new() -> Batch {
        Batch {
            buf: Box::new([0u8; BATCH_BYTES]),
            len: 0,
            count: 0,
            seg_size: 0,
            closed: false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn count(&self) -> usize {
        self.count
    }

    /// The segment size the kernel splits this batch on. Meaningful once at
    /// least one packet is in.
    pub fn seg_size(&self) -> usize {
        self.seg_size
    }

    /// Try to add `pkt`. Returns `false` if it does not belong in this batch —
    /// a size mismatch, a packet after the short final one, or the batch is
    /// full — in which case the caller flushes and adds it to a fresh batch.
    pub fn try_add(&mut self, pkt: &[u8]) -> bool {
        if pkt.is_empty() || pkt.len() > HEADER_LEN + MAX_PAYLOAD {
            return false;
        }
        if self.count == 0 {
            self.seg_size = pkt.len();
        } else if self.closed || self.count >= MAX_BATCH || pkt.len() > self.seg_size {
            return false;
        }
        self.buf[self.len..self.len + pkt.len()].copy_from_slice(pkt);
        self.len += pkt.len();
        self.count += 1;
        // A short packet can only be the last segment of a USO batch.
        if pkt.len() < self.seg_size {
            self.closed = true;
        }
        true
    }

    /// The batch's bytes and the segment size to split on.
    pub fn bytes(&self) -> (&[u8], usize) {
        (&self.buf[..self.len], self.seg_size)
    }

    /// Empty the batch for reuse.
    pub fn clear(&mut self) {
        self.len = 0;
        self.count = 0;
        self.seg_size = 0;
        self.closed = false;
    }
}

// ===========================================================================
// Sender
// ===========================================================================

/// How a batch reaches the wire. Split out so the platform-specific USO path is
/// behind one seam and the send loop is the same on every target.
pub trait Sender {
    /// Send `buf` as datagrams of `seg_size` bytes each (the last may be
    /// shorter) to `addr`. A plain sender loops `send_to`; a USO sender hands it
    /// to the kernel in one call.
    fn send_batch(&mut self, buf: &[u8], seg_size: usize, addr: SocketAddr) -> io::Result<()>;

    /// Whether the batches are actually being offloaded, for the metrics line.
    fn offloaded(&self) -> bool {
        false
    }
}

/// One `send_to` per datagram. The portable path and the runtime fallback.
pub struct PlainSender<'a> {
    socket: &'a UdpSocket,
}

impl<'a> PlainSender<'a> {
    pub fn new(socket: &'a UdpSocket) -> PlainSender<'a> {
        PlainSender { socket }
    }
}

impl Sender for PlainSender<'_> {
    fn send_batch(&mut self, buf: &[u8], seg_size: usize, addr: SocketAddr) -> io::Result<()> {
        let seg = seg_size.max(1);
        for chunk in buf.chunks(seg) {
            self.socket.send_to(chunk, addr)?;
        }
        Ok(())
    }
}

#[cfg(windows)]
pub mod windows;

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    #[test]
    fn the_pacer_spreads_a_frame_across_the_expected_time() {
        // 300 KB at 300 Mbps should take 300_000*8/300e6 = 8 ms.
        let rate = 300_000_000u64;
        let mut p = Pacer::new(rate, 0, 0);
        let mut now = 0u64;
        let mut sent = 0usize;
        // 250 packets of 1200 bytes.
        for _ in 0..250 {
            now += p.wait_ns(now);
            p.record(1200, now);
            sent += 1200;
        }
        let expected = (sent as u64 * 8 * 1_000_000_000) / rate;
        // The last record pushes availability one packet past the send, so the
        // spread is within a packet of the ideal.
        assert!(
            now >= expected - 40_000 && now <= expected + 40_000,
            "spread {now}ns vs {expected}ns"
        );
        assert!((7 * MS..=9 * MS).contains(&now), "{now}ns");
    }

    #[test]
    fn the_pacer_never_exceeds_the_rate_over_a_window() {
        let rate = 100_000_000u64; // 100 Mbps
        let mut p = Pacer::new(rate, 0, 0);
        let mut now = 0u64;
        let mut bytes = 0usize;
        for _ in 0..1000 {
            let w = p.wait_ns(now);
            now += w;
            p.record(1200, now);
            bytes += 1200;
        }
        // Bits sent divided by elapsed time must not exceed the rate.
        let achieved = bytes as u64 * 8 * 1_000_000_000 / now.max(1);
        assert!(
            achieved <= rate + rate / 100,
            "achieved {achieved} > {rate}"
        );
    }

    #[test]
    fn an_idle_burst_is_forgiven_but_bounded() {
        // Burst allowance 250 us; each 1200-byte packet costs 96 us at 100 Mbps.
        let mut p = Pacer::new(100_000_000, 250 * 1000, 0);
        let now = 10 * MS; // a long idle before the first send
        assert_eq!(p.wait_ns(now), 0, "the first packet after idle goes free");
        // A handful more may burst, but the allowance is bounded: within a few
        // packets the pace forces a wait rather than banking the whole idle.
        let mut forced = false;
        for _ in 0..8 {
            if p.wait_ns(now) > 0 {
                forced = true;
                break;
            }
            p.record(1200, now);
        }
        assert!(forced, "the idle burst must be bounded, not unlimited");
    }

    #[test]
    fn a_rate_change_takes_effect_immediately() {
        let mut p = Pacer::new(100_000_000, 0, 0);
        p.record(1200, 0);
        let slow = p.wait_ns(0);
        let mut p = Pacer::new(100_000_000, 0, 0);
        p.set_rate(200_000_000);
        p.record(1200, 0);
        let fast = p.wait_ns(0);
        assert!(fast < slow, "doubling the rate should halve the wait");
    }

    fn pkt(byte: u8, len: usize) -> Vec<u8> {
        vec![byte; len]
    }

    #[test]
    fn equal_sized_packets_batch_and_a_short_one_closes_it() {
        let mut b = Batch::new();
        assert!(b.try_add(&pkt(1, 1212)));
        assert!(b.try_add(&pkt(2, 1212)));
        // A shorter packet is a valid final segment.
        assert!(b.try_add(&pkt(3, 400)));
        // Nothing may follow the short one.
        assert!(!b.try_add(&pkt(4, 1212)));
        assert_eq!(b.count(), 3);
        let (bytes, seg) = b.bytes();
        assert_eq!(seg, 1212);
        assert_eq!(bytes.len(), 1212 + 1212 + 400);
    }

    #[test]
    fn a_larger_packet_after_the_first_is_refused() {
        let mut b = Batch::new();
        assert!(b.try_add(&pkt(1, 500)));
        // Larger than the segment size cannot join.
        assert!(!b.try_add(&pkt(2, 1200)));
        assert_eq!(b.count(), 1);
    }

    #[test]
    fn a_full_batch_refuses_more() {
        let mut b = Batch::new();
        for _ in 0..MAX_BATCH {
            assert!(b.try_add(&pkt(0, 1200)));
        }
        assert!(!b.try_add(&pkt(0, 1200)));
        assert_eq!(b.count(), MAX_BATCH);
        b.clear();
        assert!(b.is_empty());
        assert!(b.try_add(&pkt(0, 1200)));
    }

    #[test]
    fn plain_sender_splits_a_batch_into_datagrams() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let addr = server.local_addr().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();

        let mut b = Batch::new();
        b.try_add(&pkt(0xAA, 1200));
        b.try_add(&pkt(0xBB, 1200));
        b.try_add(&pkt(0xCC, 300));
        let (bytes, seg) = b.bytes();

        let mut sender = PlainSender::new(&client);
        sender.send_batch(bytes, seg, addr).unwrap();
        assert!(!sender.offloaded());

        let mut buf = [0u8; 2048];
        let mut lens = Vec::new();
        for _ in 0..3 {
            let n = server.recv(&mut buf).unwrap();
            lens.push(n);
        }
        lens.sort_unstable();
        assert_eq!(lens, vec![300, 1200, 1200], "three separate datagrams");
    }
}

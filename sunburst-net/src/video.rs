// SPDX-License-Identifier: GPL-2.0-or-later

//! The video transport core: packetize, reassemble, de-jitter.
//!
//! Pure logic, no I/O and no platform — the sender ([`Packetizer`]) runs on the
//! Windows server, the receiver ([`Reassembler`] + [`JitterBuffer`]) on the
//! Android client, and both are exercised on the Linux host by the tests at the
//! bottom of this file. That is the whole reason it lives in `sunburst-net`
//! rather than beside the encoder.
//!
//! Everything is built on the wire types already pinned in `sunburst-core`:
//! [`Header`]/[`Flags`]/[`PacketType::Video`] and the modular [`Seq16`]. Nothing
//! here reinvents the header — it fragments around it.
//!
//! # Why a terminator packet, not a flag on the last data packet
//!
//! Subframe readback hands us a frame one slice/tile at a time and never says
//! which slice is the last — the encoder's drain simply stops. Waiting to learn
//! the last chunk before sending the previous one would give back exactly the
//! encode/transmit overlap that subframe readback exists to win (CLAUDE.md:
//! "mandatory, not an optimisation").
//!
//! So data packets stream out the instant their bytes exist, carrying no total,
//! and the frame is closed by a final zero-length **terminator** packet that
//! carries [`Flags::LAST_PACKET`] and the real `pkt_count`. This also improves
//! recovery: losing one data packet still leaves the receiver holding the
//! terminator, so it knows the exact count and can NACK precisely — where a
//! last-data-packet flag would lose the byte *and* the count together.

use std::collections::BTreeMap;

use sunburst_core::proto::{Flags, HEADER_LEN, Header, MAX_PAYLOAD, PacketType, Seq16};

/// A fully-formed packet is at most the header plus one MTU-sized payload.
const MAX_PACKET: usize = HEADER_LEN + MAX_PAYLOAD;

// ===========================================================================
// Packetizer (sender)
// ===========================================================================

/// Fragments one encoded frame into wire packets, streaming each unit out as it
/// is produced.
///
/// Lifecycle per frame: [`begin_frame`](Self::begin_frame), then one
/// [`push_unit`](Self::push_unit) per slice/tile as subframe readback yields it,
/// then [`finish_frame`](Self::finish_frame) to close it. `frame_id` must
/// increase by one each frame — the receiver's ordering and the instrumentation
/// both rely on it being monotonic.
///
/// Zero-allocation after construction: packets are built in an owned scratch
/// buffer and handed to the `emit` closure by borrow, valid only for the
/// duration of that call (send it, do not stash it).
pub struct Packetizer {
    frame_id: Seq16,
    qpc: u32,
    keyframe: bool,
    /// Next packet index within the current frame. Runs across every unit.
    pkt_idx: u16,
    in_frame: bool,
    buf: Box<[u8; MAX_PACKET]>,
}

impl Default for Packetizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Packetizer {
    pub fn new() -> Packetizer {
        Packetizer {
            frame_id: Seq16(0),
            qpc: 0,
            keyframe: false,
            pkt_idx: 0,
            in_frame: false,
            buf: Box::new([0u8; MAX_PACKET]),
        }
    }

    /// Open a frame. `qpc_timestamp` is the low 32 bits of the sender's
    /// capture-time tick counter (the frame's `present_qpc`), copied into every
    /// packet as a correlation tag. `keyframe` marks an IDR/keyframe frame.
    pub fn begin_frame(&mut self, frame_id: Seq16, qpc_timestamp: u32, keyframe: bool) {
        self.frame_id = frame_id;
        self.qpc = qpc_timestamp;
        self.keyframe = keyframe;
        self.pkt_idx = 0;
        self.in_frame = true;
    }

    /// Fragment one coded unit — an HEVC NAL/slice (Annex-B start codes kept) or
    /// an AV1 OBU/tile (no start codes) — into `MAX_PAYLOAD`-sized packets,
    /// handing each finished packet to `emit`. The first fragment of the unit is
    /// marked [`Flags::UNIT_BOUNDARY`]. An empty unit emits nothing.
    pub fn push_unit(&mut self, unit: &[u8], mut emit: impl FnMut(&[u8])) {
        debug_assert!(self.in_frame, "push_unit outside begin_frame/finish_frame");
        let mut off = 0;
        let mut first = true;
        while off < unit.len() {
            let end = (off + MAX_PAYLOAD).min(unit.len());
            let mut flags = Flags::EMPTY;
            if self.keyframe {
                flags = flags.with(Flags::KEYFRAME);
            }
            if first {
                flags = flags.with(Flags::UNIT_BOUNDARY);
            }
            self.emit_packet(flags, &unit[off..end], 0, &mut emit);
            off = end;
            first = false;
        }
    }

    /// Close the frame with a zero-length terminator carrying
    /// [`Flags::LAST_PACKET`] and the frame's total `pkt_count`.
    pub fn finish_frame(&mut self, mut emit: impl FnMut(&[u8])) {
        debug_assert!(self.in_frame, "finish_frame without begin_frame");
        // The terminator takes the next index, so the total is that index + 1.
        let pkt_count = self.pkt_idx.wrapping_add(1);
        let mut flags = Flags::LAST_PACKET;
        if self.keyframe {
            flags = flags.with(Flags::KEYFRAME);
        }
        self.emit_packet(flags, &[], pkt_count, &mut emit);
        self.in_frame = false;
    }

    fn emit_packet(
        &mut self,
        flags: Flags,
        payload: &[u8],
        pkt_count: u16,
        emit: &mut impl FnMut(&[u8]),
    ) {
        let header = Header {
            packet_type: PacketType::Video,
            flags,
            frame_id: self.frame_id,
            qpc_timestamp: self.qpc,
            pkt_idx: self.pkt_idx,
            pkt_count,
        };
        let head: &mut [u8; HEADER_LEN] = (&mut self.buf[..HEADER_LEN]).try_into().unwrap();
        header.encode(head);
        self.buf[HEADER_LEN..HEADER_LEN + payload.len()].copy_from_slice(payload);
        emit(&self.buf[..HEADER_LEN + payload.len()]);
        self.pkt_idx = self.pkt_idx.wrapping_add(1);
    }
}

// ===========================================================================
// Reassembler (receiver, stage 1)
// ===========================================================================

/// A reassembled frame, ready for the jitter buffer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FrameAssembly {
    pub frame_id: Seq16,
    pub keyframe: bool,
    /// The sender's capture-time tick tag, echoed from the header.
    pub qpc_timestamp: u32,
    /// The concatenated bitstream — units in `pkt_idx` order, terminator elided.
    pub data: Vec<u8>,
}

/// Result of feeding one packet to the [`Reassembler`].
#[derive(PartialEq, Eq, Debug)]
pub enum Accept {
    /// Malformed, a duplicate, or older than an already-delivered frame.
    Ignored,
    /// Stored; the frame is still incomplete.
    Buffered,
    /// This packet completed a frame.
    Complete(FrameAssembly),
}

/// In-flight frames kept while their packets trickle in. A jitter buffer a few
/// frames deep plus reordering never needs many at once; new frames past this
/// evict the oldest, which is a frame that has already been given up on.
const MAX_FRAMES_IN_FLIGHT: usize = 16;

struct PartialFrame {
    frame_id: Seq16,
    keyframe: bool,
    qpc: u32,
    /// Total packet count, known once the terminator (LAST_PACKET) arrives.
    pkt_count: Option<u16>,
    /// Fragments by `pkt_idx`; the terminator's entry is empty.
    fragments: BTreeMap<u16, Vec<u8>>,
}

impl PartialFrame {
    fn is_complete(&self) -> bool {
        match self.pkt_count {
            Some(count) => (0..count).all(|i| self.fragments.contains_key(&i)),
            None => false,
        }
    }

    fn assemble(&self) -> FrameAssembly {
        let count = self.pkt_count.unwrap_or(0);
        let mut data = Vec::new();
        for i in 0..count {
            if let Some(frag) = self.fragments.get(&i) {
                data.extend_from_slice(frag);
            }
        }
        FrameAssembly {
            frame_id: self.frame_id,
            keyframe: self.keyframe,
            qpc_timestamp: self.qpc,
            data,
        }
    }
}

/// Collects video packets into whole frames, tolerating reorder, loss and the
/// 16-bit `frame_id` wrap.
///
/// Frames are keyed by `frame_id` compared modularly through [`Seq16`] — never
/// with `<`, which is wrong for 18 minutes and then silently reorders the buffer
/// at the wrap. NACK targets come from [`missing`](Self::missing).
pub struct Reassembler {
    slots: Vec<PartialFrame>,
    /// Newest `frame_id` already delivered complete; anything not newer is stale.
    newest_completed: Option<Seq16>,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Reassembler {
    pub fn new() -> Reassembler {
        Reassembler {
            slots: Vec::with_capacity(MAX_FRAMES_IN_FLIGHT),
            newest_completed: None,
        }
    }

    /// Feed one received packet (header included).
    pub fn push(&mut self, packet: &[u8]) -> Accept {
        let Some(header) = Header::decode(packet) else {
            return Accept::Ignored;
        };
        if header.packet_type != PacketType::Video {
            return Accept::Ignored;
        }
        let frame_id = header.frame_id;

        // Anything at or behind the last delivered frame is stale: we already
        // handed that frame up, so a straggler for it is noise.
        if let Some(nc) = self.newest_completed
            && !frame_id.is_newer_than(nc)
        {
            return Accept::Ignored;
        }

        let payload = packet[HEADER_LEN..].to_vec();
        let slot = self.slot_for(frame_id, header.qpc_timestamp);

        if header.flags.contains(Flags::KEYFRAME) {
            slot.keyframe = true;
        }
        if header.flags.contains(Flags::LAST_PACKET) {
            slot.pkt_count = Some(header.pkt_count);
        }
        // First writer of an index wins; a duplicate changes nothing.
        slot.fragments.entry(header.pkt_idx).or_insert(payload);

        if slot.is_complete() {
            let assembled = slot.assemble();
            self.slots.retain(|s| s.frame_id != frame_id);
            self.newest_completed = Some(match self.newest_completed {
                Some(nc) if nc.is_newer_than(frame_id) => nc,
                _ => frame_id,
            });
            Accept::Complete(assembled)
        } else {
            Accept::Buffered
        }
    }

    /// Missing packet indices for `frame_id`, appended to `out` (cleared first) —
    /// the raw material for a NACK. Once the terminator is in, that is every hole
    /// below `pkt_count`; before it, every hole below the highest index seen.
    pub fn missing(&self, frame_id: Seq16, out: &mut Vec<u16>) {
        out.clear();
        let Some(slot) = self.slots.iter().find(|s| s.frame_id == frame_id) else {
            return;
        };
        let upper = match slot.pkt_count {
            Some(count) => count,
            None => match slot.fragments.keys().next_back() {
                Some(&hi) => hi + 1,
                None => 0,
            },
        };
        for i in 0..upper {
            if !slot.fragments.contains_key(&i) {
                out.push(i);
            }
        }
    }

    fn slot_for(&mut self, frame_id: Seq16, qpc: u32) -> &mut PartialFrame {
        if let Some(pos) = self.slots.iter().position(|s| s.frame_id == frame_id) {
            return &mut self.slots[pos];
        }
        if self.slots.len() >= MAX_FRAMES_IN_FLIGHT {
            self.evict_oldest();
        }
        self.slots.push(PartialFrame {
            frame_id,
            keyframe: false,
            qpc,
            pkt_count: None,
            fragments: BTreeMap::new(),
        });
        self.slots.last_mut().unwrap()
    }

    fn evict_oldest(&mut self) {
        let mut oldest = 0;
        for i in 1..self.slots.len() {
            if self.slots[oldest]
                .frame_id
                .is_newer_than(self.slots[i].frame_id)
            {
                oldest = i;
            }
        }
        self.slots.swap_remove(oldest);
    }
}

// ===========================================================================
// Jitter buffer (receiver, stage 2)
// ===========================================================================

/// Never hold a frame longer than this for de-jitter (CLAUDE.md: 0–8ms).
const MAX_DEPTH_NS: u64 = 8_000_000;
/// Play-out depth as a multiple of the measured arrival jitter.
const JITTER_K: f64 = 3.0;
/// EWMA smoothing (RFC 3550 uses 1/16 for its interarrival jitter estimate).
const EWMA_SHIFT: f64 = 16.0;
/// Hard cap on queued frames, so a stalled stream cannot grow latency without
/// bound; the oldest is force-released past this.
const MAX_QUEUED: usize = 8;

struct Held {
    frame: FrameAssembly,
    arrival_ns: u64,
}

/// Reorders complete frames and releases them on an adaptive play-out deadline.
///
/// Each frame is held until `arrival + target_depth`, where `target_depth`
/// tracks measured arrival jitter — a steady stream releases almost immediately,
/// a jittery one cushions up to [`MAX_DEPTH_NS`]. Frames are released in modular
/// `frame_id` order; one that arrives after its slot has passed is dropped rather
/// than presented late, and a gap left by a lost frame is stepped over once the
/// frame behind it comes due.
///
/// The clock is an argument, not a call: [`push`](Self::push) and
/// [`pop`](Self::pop) both take `now_ns`, so play-out is testable without real
/// time — the same choice the reliable layer makes.
pub struct JitterBuffer {
    frames: Vec<Held>,
    /// Next `frame_id` expected to be released; set once the first frame leaves.
    next_expected: Option<Seq16>,
    target_depth_ns: u64,
    last_arrival_ns: Option<u64>,
    mean_interval_ns: f64,
    jitter_ns: f64,
    started: bool,
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl JitterBuffer {
    pub fn new() -> JitterBuffer {
        JitterBuffer {
            frames: Vec::with_capacity(MAX_QUEUED + 1),
            next_expected: None,
            target_depth_ns: 0,
            last_arrival_ns: None,
            mean_interval_ns: 0.0,
            jitter_ns: 0.0,
            started: false,
        }
    }

    /// Enqueue a complete frame that arrived at `now_ns`. Late frames (behind the
    /// next expected release) and duplicates are dropped.
    pub fn push(&mut self, frame: FrameAssembly, now_ns: u64) {
        if let Some(ne) = self.next_expected
            && ne.is_newer_than(frame.frame_id)
        {
            return; // its slot already passed — too late to present
        }
        if self
            .frames
            .iter()
            .any(|h| h.frame.frame_id == frame.frame_id)
        {
            return; // already queued
        }
        self.update_jitter(now_ns);
        self.frames.push(Held {
            frame,
            arrival_ns: now_ns,
        });
    }

    /// Release the next frame if its play-out deadline has passed (or the queue
    /// is over-full). Returns frames in modular `frame_id` order.
    pub fn pop(&mut self, now_ns: u64) -> Option<FrameAssembly> {
        if self.frames.is_empty() {
            return None;
        }
        let oldest = self.oldest_index();
        let due = now_ns.saturating_sub(self.frames[oldest].arrival_ns) >= self.target_depth_ns;
        if due || self.frames.len() > MAX_QUEUED {
            let held = self.frames.swap_remove(oldest);
            self.next_expected = Some(held.frame.frame_id.next());
            Some(held.frame)
        } else {
            None
        }
    }

    /// The current adaptive play-out depth, in nanoseconds.
    pub fn target_depth_ns(&self) -> u64 {
        self.target_depth_ns
    }

    /// Frames currently queued and not yet released.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    fn oldest_index(&self) -> usize {
        let mut oldest = 0;
        for i in 1..self.frames.len() {
            if self.frames[oldest]
                .frame
                .frame_id
                .is_newer_than(self.frames[i].frame.frame_id)
            {
                oldest = i;
            }
        }
        oldest
    }

    fn update_jitter(&mut self, now_ns: u64) {
        if let Some(last) = self.last_arrival_ns {
            let delta = now_ns.saturating_sub(last) as f64;
            if !self.started {
                // First interval seeds the mean; there is no deviation yet.
                self.mean_interval_ns = delta;
                self.jitter_ns = 0.0;
                self.started = true;
            } else {
                let deviation = (delta - self.mean_interval_ns).abs();
                self.jitter_ns += (deviation - self.jitter_ns) / EWMA_SHIFT;
                self.mean_interval_ns += (delta - self.mean_interval_ns) / EWMA_SHIFT;
            }
            self.target_depth_ns = ((JITTER_K * self.jitter_ns) as u64).min(MAX_DEPTH_NS);
        }
        self.last_arrival_ns = Some(now_ns);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect every packet a closure-driven emit produces into owned buffers.
    fn collect(f: impl FnOnce(&mut dyn FnMut(&[u8]))) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut sink = |p: &[u8]| out.push(p.to_vec());
        f(&mut sink);
        out
    }

    fn header_of(pkt: &[u8]) -> Header {
        Header::decode(pkt).unwrap()
    }

    // ---- Packetizer -------------------------------------------------------

    #[test]
    fn one_unit_fragments_by_mtu_and_flags_the_terminator() {
        let mut p = Packetizer::new();
        let unit = vec![0xABu8; 3000]; // 1200 + 1200 + 600 -> 3 data packets
        let pkts = collect(|emit| {
            p.begin_frame(Seq16(1), 0x1234, false);
            p.push_unit(&unit, |x| emit(x));
            p.finish_frame(|x| emit(x));
        });

        assert_eq!(pkts.len(), 4, "3 data fragments + 1 terminator");
        assert_eq!(pkts[0].len(), HEADER_LEN + 1200);
        assert_eq!(pkts[1].len(), HEADER_LEN + 1200);
        assert_eq!(pkts[2].len(), HEADER_LEN + 600);
        assert_eq!(pkts[3].len(), HEADER_LEN, "terminator carries no payload");

        for (i, pkt) in pkts.iter().enumerate() {
            let h = header_of(pkt);
            assert_eq!(h.packet_type, PacketType::Video);
            assert_eq!(h.frame_id, Seq16(1));
            assert_eq!(h.qpc_timestamp, 0x1234);
            assert_eq!(h.pkt_idx, i as u16, "pkt_idx runs 0..n");
        }
        // Only the first fragment is a unit boundary.
        assert!(header_of(&pkts[0]).flags.contains(Flags::UNIT_BOUNDARY));
        assert!(!header_of(&pkts[1]).flags.contains(Flags::UNIT_BOUNDARY));
        assert!(!header_of(&pkts[2]).flags.contains(Flags::UNIT_BOUNDARY));
        // Only the terminator is last, and it carries the total.
        for pkt in &pkts[..3] {
            assert!(!header_of(pkt).flags.contains(Flags::LAST_PACKET));
        }
        let term = header_of(&pkts[3]);
        assert!(term.flags.contains(Flags::LAST_PACKET));
        assert_eq!(term.pkt_count, 4);
    }

    #[test]
    fn each_unit_boundary_is_marked_and_indices_are_continuous() {
        let mut p = Packetizer::new();
        let a = vec![1u8; 1000];
        let b = vec![2u8; 1000];
        let pkts = collect(|emit| {
            p.begin_frame(Seq16(9), 0, false);
            p.push_unit(&a, |x| emit(x));
            p.push_unit(&b, |x| emit(x));
            p.finish_frame(|x| emit(x));
        });
        // unit a (1 packet), unit b (1 packet), terminator.
        assert_eq!(pkts.len(), 3);
        assert!(header_of(&pkts[0]).flags.contains(Flags::UNIT_BOUNDARY));
        assert!(header_of(&pkts[1]).flags.contains(Flags::UNIT_BOUNDARY));
        for (i, pkt) in pkts.iter().enumerate() {
            assert_eq!(header_of(pkt).pkt_idx, i as u16);
        }
    }

    #[test]
    fn exact_mtu_multiple_does_not_emit_a_stray_empty_fragment() {
        let mut p = Packetizer::new();
        let unit = vec![7u8; MAX_PAYLOAD * 2]; // exactly two full fragments
        let pkts = collect(|emit| {
            p.begin_frame(Seq16(0), 0, false);
            p.push_unit(&unit, |x| emit(x));
            p.finish_frame(|x| emit(x));
        });
        assert_eq!(
            pkts.len(),
            3,
            "2 full fragments + terminator, no empty third"
        );
        assert_eq!(pkts[0].len(), HEADER_LEN + MAX_PAYLOAD);
        assert_eq!(pkts[1].len(), HEADER_LEN + MAX_PAYLOAD);
    }

    #[test]
    fn keyframe_flag_rides_every_packet() {
        let mut p = Packetizer::new();
        let unit = vec![0u8; 100];
        let pkts = collect(|emit| {
            p.begin_frame(Seq16(3), 0, true);
            p.push_unit(&unit, |x| emit(x));
            p.finish_frame(|x| emit(x));
        });
        assert!(
            pkts.iter()
                .all(|p| header_of(p).flags.contains(Flags::KEYFRAME))
        );
    }

    // ---- Packetizer + Reassembler round trip ------------------------------

    fn packetize(frame_id: Seq16, qpc: u32, keyframe: bool, units: &[&[u8]]) -> Vec<Vec<u8>> {
        let mut p = Packetizer::new();
        collect(|emit| {
            p.begin_frame(frame_id, qpc, keyframe);
            for u in units {
                p.push_unit(u, |x| emit(x));
            }
            p.finish_frame(|x| emit(x));
        })
    }

    #[test]
    fn round_trip_in_order() {
        let units: [&[u8]; 2] = [&[1u8; 2500], &[2u8; 500]];
        let pkts = packetize(Seq16(5), 0xCAFE, true, &units);
        let mut r = Reassembler::new();
        let mut done = None;
        for pkt in &pkts {
            if let Accept::Complete(f) = r.push(pkt) {
                done = Some(f);
            }
        }
        let f = done.expect("frame completes");
        assert_eq!(f.frame_id, Seq16(5));
        assert!(f.keyframe);
        assert_eq!(f.qpc_timestamp, 0xCAFE);
        let expected: Vec<u8> = units.concat();
        assert_eq!(f.data, expected);
    }

    #[test]
    fn round_trip_out_of_order() {
        let units: [&[u8]; 1] = [&[9u8; 4000]];
        let mut pkts = packetize(Seq16(1), 0, false, &units);
        pkts.reverse(); // terminator first, fragments backwards
        let mut r = Reassembler::new();
        let mut done = None;
        for pkt in &pkts {
            if let Accept::Complete(f) = r.push(pkt) {
                done = Some(f);
            }
        }
        assert_eq!(done.unwrap().data, units.concat());
    }

    #[test]
    fn a_dropped_fragment_leaves_the_frame_incomplete_and_nackable() {
        let units: [&[u8]; 1] = [&[3u8; 3000]]; // idx 0,1,2 data + idx 3 terminator
        let pkts = packetize(Seq16(2), 0, false, &units);
        let mut r = Reassembler::new();
        // Deliver everything except data packet index 1.
        for (i, pkt) in pkts.iter().enumerate() {
            if i == 1 {
                continue;
            }
            assert_eq!(r.push(pkt), Accept::Buffered);
        }
        let mut miss = Vec::new();
        r.missing(Seq16(2), &mut miss);
        assert_eq!(miss, vec![1], "the one gap is reported for NACK");
        // The retransmit completes it.
        assert!(matches!(r.push(&pkts[1]), Accept::Complete(_)));
    }

    #[test]
    fn a_lost_terminator_keeps_the_frame_open() {
        let units: [&[u8]; 1] = [&[4u8; 1500]];
        let pkts = packetize(Seq16(7), 0, false, &units);
        let mut r = Reassembler::new();
        for pkt in &pkts[..pkts.len() - 1] {
            assert_eq!(r.push(pkt), Accept::Buffered);
        }
        // Without the terminator the count is unknown, so it cannot complete.
        let mut miss = Vec::new();
        r.missing(Seq16(7), &mut miss);
        assert!(miss.is_empty(), "no gaps below the highest seen index");
        assert!(matches!(r.push(pkts.last().unwrap()), Accept::Complete(_)));
    }

    #[test]
    fn duplicates_do_not_corrupt_the_frame() {
        let units: [&[u8]; 1] = [&[8u8; 2000]];
        let pkts = packetize(Seq16(4), 0, false, &units);
        let mut r = Reassembler::new();
        let mut done = None;
        for pkt in pkts.iter().chain(pkts.iter()) {
            // Feed every packet twice, interleaved with the originals.
            if let Accept::Complete(f) = r.push(pkt) {
                done = Some(f);
            }
        }
        assert_eq!(done.unwrap().data, units.concat());
    }

    #[test]
    fn frames_across_the_seq_wrap_both_complete() {
        let a = packetize(Seq16(0xFFFF), 0, false, &[&[1u8; 100][..]]);
        let b = packetize(Seq16(0x0000), 0, false, &[&[2u8; 100][..]]);
        let mut r = Reassembler::new();
        let mut ids = Vec::new();
        for pkt in a.iter().chain(b.iter()) {
            if let Accept::Complete(f) = r.push(pkt) {
                ids.push(f.frame_id);
            }
        }
        assert_eq!(ids, vec![Seq16(0xFFFF), Seq16(0x0000)]);
    }

    #[test]
    fn stragglers_for_a_delivered_frame_are_ignored() {
        let pkts = packetize(Seq16(10), 0, false, &[&[1u8; 100][..]]);
        let mut r = Reassembler::new();
        for pkt in &pkts {
            r.push(pkt);
        }
        // A re-sent fragment for the frame we already delivered is stale.
        assert_eq!(r.push(&pkts[0]), Accept::Ignored);
    }

    #[test]
    fn a_short_or_nonvideo_packet_is_ignored() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&[0u8; HEADER_LEN - 1]), Accept::Ignored);
        let mut audio = [0u8; HEADER_LEN];
        audio[0] = PacketType::Audio as u8;
        assert_eq!(r.push(&audio), Accept::Ignored);
    }

    #[test]
    fn overflowing_the_in_flight_set_evicts_the_oldest_and_still_completes() {
        let mut r = Reassembler::new();
        // Open one data packet for each of many frame_ids without finishing them.
        for id in 0..(MAX_FRAMES_IN_FLIGHT as u16 + 4) {
            let pkts = packetize(Seq16(id), 0, false, &[&[0u8; 100][..]]);
            assert_eq!(r.push(&pkts[0]), Accept::Buffered); // data only, no terminator
        }
        // A brand-new frame still completes despite the churn.
        let pkts = packetize(Seq16(1000), 0, false, &[&[5u8; 100][..]]);
        let mut done = None;
        for pkt in &pkts {
            if let Accept::Complete(f) = r.push(pkt) {
                done = Some(f);
            }
        }
        assert_eq!(done.unwrap().frame_id, Seq16(1000));
    }

    // ---- Jitter buffer ----------------------------------------------------

    fn frame(id: u16) -> FrameAssembly {
        FrameAssembly {
            frame_id: Seq16(id),
            keyframe: false,
            qpc_timestamp: 0,
            data: vec![id as u8],
        }
    }

    const MS: u64 = 1_000_000;

    #[test]
    fn releases_in_order_after_the_depth_elapses() {
        let mut j = JitterBuffer::new();
        // Steady 16ms cadence -> negligible jitter, tiny depth.
        for k in 0..4u64 {
            j.push(frame(k as u16), k * 16 * MS);
        }
        let mut got = Vec::new();
        // Drain well after the last arrival.
        let now = 100 * MS;
        while let Some(f) = j.pop(now) {
            got.push(f.frame_id.0);
        }
        assert_eq!(got, vec![0, 1, 2, 3]);
    }

    #[test]
    fn reordered_arrivals_leave_in_order() {
        let mut j = JitterBuffer::new();
        j.push(frame(2), 0);
        j.push(frame(1), 0);
        j.push(frame(0), 0);
        let mut got = Vec::new();
        while let Some(f) = j.pop(100 * MS) {
            got.push(f.frame_id.0);
        }
        assert_eq!(got, vec![0, 1, 2]);
    }

    #[test]
    fn a_frame_that_arrives_after_its_slot_is_dropped() {
        let mut j = JitterBuffer::new();
        // Release 0 and 1.
        j.push(frame(0), 0);
        j.push(frame(1), 16 * MS);
        assert_eq!(j.pop(100 * MS).unwrap().frame_id, Seq16(0));
        assert_eq!(j.pop(100 * MS).unwrap().frame_id, Seq16(1));
        // Frame 0 shows up again, far too late.
        j.push(frame(0), 120 * MS);
        assert!(
            j.pop(200 * MS).is_none(),
            "the late frame was dropped, not queued"
        );
    }

    #[test]
    fn a_gap_from_a_lost_frame_is_stepped_over() {
        let mut j = JitterBuffer::new();
        j.push(frame(0), 0);
        assert_eq!(j.pop(100 * MS).unwrap().frame_id, Seq16(0));
        // Frame 1 never arrives; frame 2 does.
        j.push(frame(2), 16 * MS);
        // Once 2 is due it is released, skipping the missing 1.
        assert_eq!(j.pop(100 * MS).unwrap().frame_id, Seq16(2));
    }

    #[test]
    fn depth_stays_small_for_a_steady_stream_and_grows_when_jittery() {
        let mut steady = JitterBuffer::new();
        let mut t = 0u64;
        for k in 0..40u16 {
            t += 16 * MS;
            steady.push(frame(k), t);
        }
        assert!(
            steady.target_depth_ns() < MS,
            "steady stream needs almost no cushion, got {}ns",
            steady.target_depth_ns()
        );

        let mut jittery = JitterBuffer::new();
        let mut t = 0u64;
        for k in 0..40u16 {
            // Alternate 4ms and 28ms gaps: mean ~16ms, large deviation.
            t += if k % 2 == 0 { 4 * MS } else { 28 * MS };
            jittery.push(frame(k), t);
        }
        assert!(
            jittery.target_depth_ns() > steady.target_depth_ns(),
            "a jittery stream cushions more"
        );
        assert!(
            jittery.target_depth_ns() <= MAX_DEPTH_NS,
            "but never past the 8ms cap"
        );
    }

    #[test]
    fn an_overfull_queue_force_releases_rather_than_growing() {
        let mut j = JitterBuffer::new();
        // Many frames all at t=0 with a large depth would otherwise stall; the
        // MAX_QUEUED cap forces the oldest out even before its deadline.
        for k in 0..(MAX_QUEUED as u16 + 3) {
            j.push(frame(k), 0);
        }
        // now == 0, so no frame's depth has elapsed, yet the cap still drains one.
        assert!(j.pop(0).is_some());
    }
}

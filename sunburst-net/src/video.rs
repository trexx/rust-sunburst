// SPDX-License-Identifier: GPL-2.0-or-later

//! The video transport core: packetize, reassemble, de-jitter.
//!
//! Pure logic, no I/O and no platform — the sender ([`Packetizer`]) runs on the
//! Windows server, the receiver ([`Reassembler`] + [`JitterBuffer`]) on the
//! Android client, and both are exercised on the Linux host by the tests at the
//! bottom of this file. That is the whole reason it lives in `sunburst-net`
//! rather than beside the encoder.
//!
//! Both halves are hot path (CLAUDE.md rule 1): after construction nothing here
//! allocates. The receiver keeps payloads in a fixed [pool](PacketPool) and
//! hands out [`FrameRef`] handles the decoder copies from directly, so a frame
//! is never concatenated on the way through. `tests/zero_alloc_receive.rs`
//! holds that to a counting allocator.
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
// Packet pool (receiver storage)
// ===========================================================================

/// Fixed payload slots shared by every in-flight frame. Allocated once, at
/// construction; afterwards a packet costs one copy into a slot and nothing
/// else. The header has already been decoded on the way in, so a slot holds a
/// payload and its length.
struct PacketPool {
    data: Box<[[u8; MAX_PAYLOAD]]>,
    lens: Box<[u16]>,
    /// Free slots as a stack, sized to hold every slot so it never grows.
    free: Box<[u32]>,
    free_top: usize,
}

impl PacketPool {
    fn new(slots: usize) -> PacketPool {
        let slots = slots.max(1);
        // Reversed so the first `take` hands out slot 0 — cosmetic, but it makes
        // a debugger's view of the pool read in order.
        let free: Vec<u32> = (0..slots as u32).rev().collect();
        PacketPool {
            data: vec![[0u8; MAX_PAYLOAD]; slots].into_boxed_slice(),
            lens: vec![0u16; slots].into_boxed_slice(),
            free: free.into_boxed_slice(),
            free_top: slots,
        }
    }

    fn take(&mut self, payload: &[u8]) -> Option<u32> {
        if self.free_top == 0 || payload.len() > MAX_PAYLOAD {
            return None;
        }
        self.free_top -= 1;
        let slot = self.free[self.free_top];
        self.data[slot as usize][..payload.len()].copy_from_slice(payload);
        self.lens[slot as usize] = payload.len() as u16;
        Some(slot)
    }

    fn give(&mut self, slot: u32) {
        debug_assert!(
            self.free_top < self.free.len(),
            "double free of a pool slot"
        );
        self.free[self.free_top] = slot;
        self.free_top += 1;
    }

    fn get(&self, slot: u32) -> &[u8] {
        &self.data[slot as usize][..self.lens[slot as usize] as usize]
    }

    fn available(&self) -> usize {
        self.free_top
    }
}

// ===========================================================================
// Reassembler (receiver, stage 1)
// ===========================================================================

/// A completed frame, held by the [`Reassembler`] until it is
/// [`release`](Reassembler::release)d. Deliberately not `Clone`: one handle,
/// one release, and the compiler enforces that a moved-out handle is gone.
///
/// The bytes are read through [`Reassembler::copy_into`] or
/// [`Reassembler::fragments`] — straight from the pool into the decoder's own
/// input buffer, so nothing is concatenated on the way.
#[derive(PartialEq, Eq, Debug)]
pub struct FrameRef {
    pub frame_id: Seq16,
    pub keyframe: bool,
    /// The sender's capture-time tick tag, echoed from the header.
    pub qpc_timestamp: u32,
    /// Total bitstream bytes (terminator excluded).
    pub len: usize,
    slot: u32,
    /// Generation of the slot when this handle was issued; a stale handle
    /// (released once already, slot reused) is refused rather than freeing a
    /// newer frame's storage.
    token: u32,
}

/// Result of feeding one packet to the [`Reassembler`].
#[derive(PartialEq, Eq, Debug)]
pub enum Accept {
    /// Malformed, a duplicate, older than an already-delivered frame, or
    /// dropped because no storage could be found for it.
    Ignored,
    /// Stored; the frame is still incomplete.
    Buffered,
    /// This packet completed a frame.
    Complete(FrameRef),
}

/// Frames tracked at once: those still arriving plus those completed and held
/// for the jitter buffer. A jitter buffer a few frames deep plus reordering
/// never needs many; past this the oldest partial frame is abandoned.
pub const MAX_FRAMES_IN_FLIGHT: usize = 24;

/// Most packets one frame may span. A 4K keyframe at 150 Mbps is one to three
/// thousand; the index table is sized so the terminator can never point past it.
pub const MAX_PKTS_PER_FRAME: usize = 8192;

/// Default pool: about 10 MB, which is several keyframes or a couple of dozen
/// P-frames — more than the frames-in-flight bound can ever pin.
pub const DEFAULT_POOL_SLOTS: usize = 8192;

/// How many abandoned frame ids are remembered between
/// [`drain_abandoned`](Reassembler::drain_abandoned) calls. Older ones fall off:
/// an abandon means "this frame and everything since", so the oldest is the one
/// that matters and it is kept.
const ABANDON_LOG: usize = 16;

const NO_SLOT: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SlotState {
    Free,
    /// Packets still arriving.
    Partial,
    /// Complete and handed out as a [`FrameRef`]; owned by the caller.
    Held,
}

struct FrameSlot {
    state: SlotState,
    frame_id: Seq16,
    keyframe: bool,
    qpc: u32,
    /// Total packet count, known once the terminator (LAST_PACKET) arrives.
    pkt_count: Option<u16>,
    /// Distinct indices stored so far.
    received: u16,
    /// One past the highest index stored, so a reset only clears what was used.
    high_water: usize,
    /// Bitstream bytes, set when the frame completes.
    len: usize,
    token: u32,
    /// `pkt_idx` → pool slot, or [`NO_SLOT`].
    index: Box<[u32]>,
}

impl FrameSlot {
    fn empty() -> FrameSlot {
        FrameSlot {
            state: SlotState::Free,
            frame_id: Seq16(0),
            keyframe: false,
            qpc: 0,
            pkt_count: None,
            received: 0,
            high_water: 0,
            len: 0,
            token: 0,
            index: vec![NO_SLOT; MAX_PKTS_PER_FRAME].into_boxed_slice(),
        }
    }

    /// Return every pool slot and reset for reuse.
    fn clear(&mut self, pool: &mut PacketPool) {
        for entry in &mut self.index[..self.high_water] {
            if *entry != NO_SLOT {
                pool.give(*entry);
                *entry = NO_SLOT;
            }
        }
        self.state = SlotState::Free;
        self.pkt_count = None;
        self.received = 0;
        self.high_water = 0;
        self.len = 0;
        self.keyframe = false;
    }

    /// Complete means every index below the terminator's count is present;
    /// returns the bitstream length when it is. The count is a fast pre-check;
    /// the scan is what makes a stray index at or above `pkt_count` unable to
    /// fake completion, and summing on the same pass is what keeps such an
    /// index's bytes out of the length.
    fn complete_len(&self, pool: &PacketPool) -> Option<usize> {
        match self.pkt_count {
            Some(count) if self.received >= count => {
                let mut len = 0;
                for entry in &self.index[..count as usize] {
                    if *entry == NO_SLOT {
                        return None;
                    }
                    len += pool.get(*entry).len();
                }
                Some(len)
            }
            _ => None,
        }
    }
}

/// Collects video packets into whole frames, tolerating reorder, loss and the
/// 16-bit `frame_id` wrap, without allocating after construction.
///
/// Frames are keyed by `frame_id` compared modularly through [`Seq16`] — never
/// with `<`, which is wrong for 18 minutes and then silently reorders the buffer
/// at the wrap. NACK targets come from [`missing`](Self::missing); frames given
/// up on are reported by [`drain_abandoned`](Self::drain_abandoned) so the
/// client can tell the server to stop referencing them.
pub struct Reassembler {
    pool: PacketPool,
    frames: Box<[FrameSlot]>,
    /// Newest `frame_id` already delivered complete; anything not newer is stale.
    newest_completed: Option<Seq16>,
    abandoned: [Seq16; ABANDON_LOG],
    abandoned_len: usize,
    next_token: u32,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Reassembler {
    /// Default sizing: [`DEFAULT_POOL_SLOTS`] payloads, [`MAX_FRAMES_IN_FLIGHT`]
    /// frames.
    pub fn new() -> Reassembler {
        Self::with_capacity(DEFAULT_POOL_SLOTS, MAX_FRAMES_IN_FLIGHT)
    }

    /// Explicit sizing, for tests and memory-constrained clients.
    pub fn with_capacity(pool_slots: usize, frames: usize) -> Reassembler {
        let frames = frames.max(2);
        Reassembler {
            pool: PacketPool::new(pool_slots),
            frames: (0..frames)
                .map(|_| FrameSlot::empty())
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            newest_completed: None,
            abandoned: [Seq16(0); ABANDON_LOG],
            abandoned_len: 0,
            next_token: 1,
        }
    }

    /// Payload slots currently free. Exposed so a caller can watch for pool
    /// pressure; also what the tests use to prove releases return storage.
    pub fn pool_available(&self) -> usize {
        self.pool.available()
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
        let pkt_idx = header.pkt_idx as usize;
        if pkt_idx >= MAX_PKTS_PER_FRAME {
            return Accept::Ignored;
        }

        // Anything at or behind the last delivered frame is stale: we already
        // handed that frame up, so a straggler for it is noise.
        if let Some(nc) = self.newest_completed
            && !frame_id.is_newer_than(nc)
        {
            return Accept::Ignored;
        }

        let Some(fi) = self.slot_for(frame_id, header.qpc_timestamp) else {
            return Accept::Ignored;
        };

        if self.frames[fi].index[pkt_idx] != NO_SLOT {
            // First writer of an index wins; a duplicate changes nothing.
            return Accept::Buffered;
        }
        let payload = &packet[HEADER_LEN..];
        let pool_slot = match self.pool.take(payload) {
            Some(s) => s,
            None => {
                // Storage is the bound, not the frame count: give up the oldest
                // other partial frame and try once more.
                if !self.evict_oldest_partial(Some(fi)) {
                    // This frame is the only partial one and the pool is full of
                    // held frames: nothing to reclaim, drop the packet.
                    return Accept::Ignored;
                }
                match self.pool.take(payload) {
                    Some(s) => s,
                    None => return Accept::Ignored,
                }
            }
        };

        let slot = &mut self.frames[fi];
        slot.index[pkt_idx] = pool_slot;
        slot.high_water = slot.high_water.max(pkt_idx + 1);
        slot.received += 1;
        if header.flags.contains(Flags::KEYFRAME) {
            slot.keyframe = true;
        }
        if header.flags.contains(Flags::LAST_PACKET) {
            slot.pkt_count = Some(header.pkt_count);
        }

        if let Some(len) = slot.complete_len(&self.pool) {
            slot.len = len;
            slot.state = SlotState::Held;
            slot.token = self.next_token;
            self.next_token = self.next_token.wrapping_add(1).max(1);
            let frame = FrameRef {
                frame_id,
                keyframe: slot.keyframe,
                qpc_timestamp: slot.qpc,
                len: slot.len,
                slot: fi as u32,
                token: slot.token,
            };
            self.newest_completed = Some(match self.newest_completed {
                Some(nc) if nc.is_newer_than(frame_id) => nc,
                _ => frame_id,
            });
            Accept::Complete(frame)
        } else {
            Accept::Buffered
        }
    }

    /// Missing packet indices for `frame_id`, written to `out`; returns how
    /// many (at most `out.len()`) — the raw material for a NACK. Once the
    /// terminator is in, that is every hole below `pkt_count`; before it, every
    /// hole below the highest index seen.
    pub fn missing(&self, frame_id: Seq16, out: &mut [u16]) -> usize {
        let Some(slot) = self
            .frames
            .iter()
            .find(|s| s.state == SlotState::Partial && s.frame_id == frame_id)
        else {
            return 0;
        };
        let upper = match slot.pkt_count {
            Some(count) => count as usize,
            None => slot.high_water,
        };
        let mut n = 0;
        for (i, entry) in slot.index[..upper].iter().enumerate() {
            if *entry == NO_SLOT {
                if n == out.len() {
                    break;
                }
                out[n] = i as u16;
                n += 1;
            }
        }
        n
    }

    /// Whether `frame_id` is still being assembled.
    pub fn is_pending(&self, frame_id: Seq16) -> bool {
        self.frames
            .iter()
            .any(|s| s.state == SlotState::Partial && s.frame_id == frame_id)
    }

    /// Frame ids given up on since the last call, oldest first, written to
    /// `out`; returns how many. The client sends each as an abandon-NACK.
    pub fn drain_abandoned(&mut self, out: &mut [Seq16]) -> usize {
        let n = self.abandoned_len.min(out.len());
        out[..n].copy_from_slice(&self.abandoned[..n]);
        self.abandoned.copy_within(n..self.abandoned_len, 0);
        self.abandoned_len -= n;
        n
    }

    /// Give up on `frame_id` now (the jitter buffer has moved past it), freeing
    /// its storage. It is *not* logged as abandoned — the caller already knows.
    pub fn discard(&mut self, frame_id: Seq16) {
        if let Some(fi) = self
            .frames
            .iter()
            .position(|s| s.state == SlotState::Partial && s.frame_id == frame_id)
        {
            self.frames[fi].clear(&mut self.pool);
        }
    }

    /// Copy the frame's bitstream into `out`, in `pkt_idx` order. Returns the
    /// bytes written, or `None` if `out` is too small or the handle is stale.
    pub fn copy_into(&self, frame: &FrameRef, out: &mut [u8]) -> Option<usize> {
        let slot = self.held(frame)?;
        if out.len() < slot.len {
            return None;
        }
        let mut at = 0;
        for entry in &slot.index[..slot.pkt_count.unwrap_or(0) as usize] {
            let frag = self.pool.get(*entry);
            out[at..at + frag.len()].copy_from_slice(frag);
            at += frag.len();
        }
        Some(at)
    }

    /// The frame's fragments in order, without copying. Empty for a stale handle.
    pub fn fragments<'a>(&'a self, frame: &FrameRef) -> impl Iterator<Item = &'a [u8]> + 'a {
        let (index, count): (&'a [u32], usize) = match self.held(frame) {
            Some(slot) => (&slot.index, slot.pkt_count.unwrap_or(0) as usize),
            None => (&[], 0),
        };
        index[..count]
            .iter()
            .map(move |entry| self.pool.get(*entry))
            .filter(|frag| !frag.is_empty())
    }

    /// Return the frame's storage. A stale handle is ignored.
    pub fn release(&mut self, frame: FrameRef) {
        let fi = frame.slot as usize;
        if let Some(slot) = self.frames.get_mut(fi)
            && slot.state == SlotState::Held
            && slot.token == frame.token
        {
            slot.clear(&mut self.pool);
        }
    }

    fn held(&self, frame: &FrameRef) -> Option<&FrameSlot> {
        let slot = self.frames.get(frame.slot as usize)?;
        (slot.state == SlotState::Held && slot.token == frame.token).then_some(slot)
    }

    /// Index of the slot assembling `frame_id`, allocating (and if necessary
    /// evicting the oldest partial frame) for a new one.
    fn slot_for(&mut self, frame_id: Seq16, qpc: u32) -> Option<usize> {
        if let Some(fi) = self
            .frames
            .iter()
            .position(|s| s.state == SlotState::Partial && s.frame_id == frame_id)
        {
            return Some(fi);
        }
        let free = match self.frames.iter().position(|s| s.state == SlotState::Free) {
            Some(fi) => fi,
            None => {
                if !self.evict_oldest_partial(None) {
                    return None; // every slot is held by the caller
                }
                self.frames
                    .iter()
                    .position(|s| s.state == SlotState::Free)?
            }
        };
        let slot = &mut self.frames[free];
        slot.state = SlotState::Partial;
        slot.frame_id = frame_id;
        slot.qpc = qpc;
        Some(free)
    }

    /// Abandon the oldest partial frame other than `except`, logging it.
    /// Returns whether anything was evicted.
    fn evict_oldest_partial(&mut self, except: Option<usize>) -> bool {
        let mut oldest: Option<usize> = None;
        for (i, s) in self.frames.iter().enumerate() {
            if s.state != SlotState::Partial || Some(i) == except {
                continue;
            }
            oldest = match oldest {
                Some(o) if !self.frames[o].frame_id.is_newer_than(s.frame_id) => Some(o),
                _ => Some(i),
            };
        }
        let Some(fi) = oldest else {
            return false;
        };
        let id = self.frames[fi].frame_id;
        self.frames[fi].clear(&mut self.pool);
        self.log_abandoned(id);
        true
    }

    fn log_abandoned(&mut self, id: Seq16) {
        if self.abandoned_len < ABANDON_LOG {
            self.abandoned[self.abandoned_len] = id;
            self.abandoned_len += 1;
        }
        // Full: the oldest entries are the ones that matter (an abandon covers
        // everything after it), so newer ones are the ones to lose.
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
    frame: FrameRef,
    arrival_ns: u64,
}

/// A frame the jitter buffer has released, and what it skipped to get there.
#[derive(PartialEq, Eq, Debug)]
pub struct Released {
    pub frame: FrameRef,
    /// The oldest frame id stepped over because it never arrived (or arrived
    /// too late). One id is enough: an abandon covers everything after it.
    pub stepped_over: Option<Seq16>,
}

/// Reorders complete frames and releases them on an adaptive play-out deadline.
///
/// Each frame is held until `arrival + target_depth`, where `target_depth`
/// tracks measured arrival jitter — a steady stream releases almost immediately,
/// a jittery one cushions up to the depth cap. Frames are released in modular
/// `frame_id` order; one that arrives after its slot has passed is refused
/// rather than presented late, and a gap left by a lost frame is stepped over
/// (and reported) once the frame behind it comes due.
///
/// The clock is an argument, not a call: [`push`](Self::push) and
/// [`pop`](Self::pop) both take `now_ns`, so play-out is testable without real
/// time — the same choice the reliable layer makes. Storage is a fixed array;
/// nothing here allocates.
pub struct JitterBuffer {
    frames: [Option<Held>; MAX_QUEUED + 1],
    queued: usize,
    /// Next `frame_id` expected to be released; set once the first frame leaves.
    next_expected: Option<Seq16>,
    target_depth_ns: u64,
    /// [`MAX_DEPTH_NS`], or half the frame interval if that is smaller — at
    /// 120 Hz an 8 ms cushion would be a whole frame.
    max_depth_ns: u64,
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
            frames: Default::default(),
            queued: 0,
            next_expected: None,
            target_depth_ns: 0,
            max_depth_ns: MAX_DEPTH_NS,
            last_arrival_ns: None,
            mean_interval_ns: 0.0,
            jitter_ns: 0.0,
            started: false,
        }
    }

    /// Tell the buffer the stream's frame interval, so the depth cap never
    /// exceeds half a frame: the 8 ms ceiling is right at 60 Hz and a whole
    /// frame late at 120 Hz.
    pub fn set_frame_interval_ns(&mut self, interval_ns: u64) {
        self.max_depth_ns = MAX_DEPTH_NS.min(interval_ns / 2).max(1);
        self.target_depth_ns = self.target_depth_ns.min(self.max_depth_ns);
    }

    /// Enqueue a complete frame that arrived at `now_ns`. A frame that is late
    /// (behind the next expected release), a duplicate, or one more than the
    /// buffer can hold is handed back so the caller can release its storage.
    pub fn push(&mut self, frame: FrameRef, now_ns: u64) -> Result<(), FrameRef> {
        if let Some(ne) = self.next_expected
            && ne.is_newer_than(frame.frame_id)
        {
            return Err(frame); // its slot already passed — too late to present
        }
        if self
            .frames
            .iter()
            .flatten()
            .any(|h| h.frame.frame_id == frame.frame_id)
        {
            return Err(frame); // already queued
        }
        let Some(free) = self.frames.iter().position(Option::is_none) else {
            return Err(frame); // the caller is not popping; do not grow
        };
        self.update_jitter(now_ns);
        self.frames[free] = Some(Held {
            frame,
            arrival_ns: now_ns,
        });
        self.queued += 1;
        Ok(())
    }

    /// Release the next frame if its play-out deadline has passed (or the queue
    /// is over-full). Returns frames in modular `frame_id` order.
    pub fn pop(&mut self, now_ns: u64) -> Option<Released> {
        let oldest = self.oldest_index()?;
        let held = self.frames[oldest].as_ref().expect("oldest_index found it");
        let due = now_ns.saturating_sub(held.arrival_ns) >= self.target_depth_ns;
        if !due && self.queued <= MAX_QUEUED {
            return None;
        }
        let held = self.frames[oldest].take().expect("checked above");
        self.queued -= 1;
        let stepped_over = match self.next_expected {
            Some(ne) if held.frame.frame_id.is_newer_than(ne) => Some(ne),
            _ => None,
        };
        self.next_expected = Some(held.frame.frame_id.next());
        Some(Released {
            frame: held.frame,
            stepped_over,
        })
    }

    /// The current adaptive play-out depth, in nanoseconds.
    pub fn target_depth_ns(&self) -> u64 {
        self.target_depth_ns
    }

    /// Frames currently queued and not yet released.
    pub fn len(&self) -> usize {
        self.queued
    }

    pub fn is_empty(&self) -> bool {
        self.queued == 0
    }

    fn oldest_index(&self) -> Option<usize> {
        let mut oldest: Option<usize> = None;
        for (i, held) in self.frames.iter().enumerate() {
            let Some(h) = held else { continue };
            oldest = match oldest {
                Some(o)
                    if !self.frames[o]
                        .as_ref()
                        .expect("occupied")
                        .frame
                        .frame_id
                        .is_newer_than(h.frame.frame_id) =>
                {
                    Some(o)
                }
                _ => Some(i),
            };
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
            self.target_depth_ns = ((JITTER_K * self.jitter_ns) as u64).min(self.max_depth_ns);
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

    /// Read a completed frame out both ways, check they agree, and release it.
    fn assemble(r: &mut Reassembler, frame: FrameRef) -> Vec<u8> {
        let mut out = vec![0u8; frame.len];
        let n = r.copy_into(&frame, &mut out).expect("a live handle copies");
        assert_eq!(n, frame.len);
        let joined: Vec<u8> = r
            .fragments(&frame)
            .flat_map(|f| f.iter().copied())
            .collect();
        assert_eq!(joined, out, "fragments() and copy_into() disagree");
        r.release(frame);
        out
    }

    fn feed(r: &mut Reassembler, pkts: &[Vec<u8>]) -> Option<FrameRef> {
        let mut done = None;
        for pkt in pkts {
            if let Accept::Complete(f) = r.push(pkt) {
                done = Some(f);
            }
        }
        done
    }

    #[test]
    fn round_trip_in_order() {
        let units: [&[u8]; 2] = [&[1u8; 2500], &[2u8; 500]];
        let pkts = packetize(Seq16(5), 0xCAFE, true, &units);
        let mut r = Reassembler::new();
        let f = feed(&mut r, &pkts).expect("frame completes");
        assert_eq!(f.frame_id, Seq16(5));
        assert!(f.keyframe);
        assert_eq!(f.qpc_timestamp, 0xCAFE);
        assert_eq!(f.len, 3000);
        let expected: Vec<u8> = units.concat();
        assert_eq!(assemble(&mut r, f), expected);
    }

    #[test]
    fn round_trip_out_of_order() {
        let units: [&[u8]; 1] = [&[9u8; 4000]];
        let mut pkts = packetize(Seq16(1), 0, false, &units);
        pkts.reverse(); // terminator first, fragments backwards
        let mut r = Reassembler::new();
        let f = feed(&mut r, &pkts).unwrap();
        assert_eq!(assemble(&mut r, f), units.concat());
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
        assert!(r.is_pending(Seq16(2)));
        let mut miss = [0u16; 8];
        let n = r.missing(Seq16(2), &mut miss);
        assert_eq!(&miss[..n], &[1], "the one gap is reported for NACK");
        // The retransmit completes it.
        let Accept::Complete(f) = r.push(&pkts[1]) else {
            panic!("retransmit did not complete the frame")
        };
        assert!(!r.is_pending(Seq16(2)));
        assert_eq!(assemble(&mut r, f), units.concat());
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
        let mut miss = [0u16; 8];
        assert_eq!(
            r.missing(Seq16(7), &mut miss),
            0,
            "no gaps below the highest seen index"
        );
        assert!(matches!(r.push(pkts.last().unwrap()), Accept::Complete(_)));
    }

    #[test]
    fn duplicates_do_not_corrupt_the_frame() {
        let units: [&[u8]; 1] = [&[8u8; 2000]];
        let pkts = packetize(Seq16(4), 0, false, &units);
        let mut r = Reassembler::new();
        let before = r.pool_available();
        let doubled: Vec<Vec<u8>> = pkts.iter().chain(pkts.iter()).cloned().collect();
        let f = feed(&mut r, &doubled).unwrap();
        assert_eq!(
            before - r.pool_available(),
            pkts.len(),
            "a duplicate must not take a second slot"
        );
        assert_eq!(assemble(&mut r, f), units.concat());
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
                r.release(f);
            }
        }
        assert_eq!(ids, vec![Seq16(0xFFFF), Seq16(0x0000)]);
    }

    #[test]
    fn stragglers_for_a_delivered_frame_are_ignored() {
        let pkts = packetize(Seq16(10), 0, false, &[&[1u8; 100][..]]);
        let mut r = Reassembler::new();
        let f = feed(&mut r, &pkts).unwrap();
        // A re-sent fragment for the frame we already delivered is stale —
        // whether the handle is still held or already released.
        assert_eq!(r.push(&pkts[0]), Accept::Ignored);
        r.release(f);
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
    fn overflowing_the_in_flight_set_abandons_the_oldest_and_still_completes() {
        let mut r = Reassembler::with_capacity(1024, 4);
        // Open one data packet for each of six frame_ids without finishing them:
        // two more than there are slots.
        for id in 0..6u16 {
            let pkts = packetize(Seq16(id), 0, false, &[&[0u8; 100][..]]);
            assert_eq!(r.push(&pkts[0]), Accept::Buffered);
        }
        let mut gone = [Seq16(0); 8];
        let n = r.drain_abandoned(&mut gone);
        assert_eq!(
            &gone[..n],
            &[Seq16(0), Seq16(1)],
            "the two oldest were given up"
        );
        assert_eq!(r.drain_abandoned(&mut gone), 0, "reported once");
        // A brand-new frame still completes despite the churn.
        let pkts = packetize(Seq16(1000), 0, false, &[&[5u8; 100][..]]);
        let f = feed(&mut r, &pkts).unwrap();
        assert_eq!(f.frame_id, Seq16(1000));
    }

    #[test]
    fn pool_exhaustion_abandons_the_oldest_partial_frame() {
        // Eight payload slots. Two partial frames of four packets each fill it;
        // a packet for a third frame must reclaim the oldest, not be dropped.
        let mut r = Reassembler::with_capacity(8, 8);
        let a = packetize(Seq16(1), 0, false, &[&[1u8; 4 * MAX_PAYLOAD][..]]);
        let b = packetize(Seq16(2), 0, false, &[&[2u8; 4 * MAX_PAYLOAD][..]]);
        for pkt in a[..4].iter().chain(b[..4].iter()) {
            assert_eq!(r.push(pkt), Accept::Buffered);
        }
        assert_eq!(r.pool_available(), 0);

        let c = packetize(Seq16(3), 0, false, &[&[3u8; 100][..]]);
        assert_eq!(r.push(&c[0]), Accept::Buffered);
        let mut gone = [Seq16(0); 8];
        let n = r.drain_abandoned(&mut gone);
        assert_eq!(&gone[..n], &[Seq16(1)]);
        assert!(!r.is_pending(Seq16(1)));
        assert!(r.is_pending(Seq16(2)));
        assert_eq!(r.pool_available(), 8 - 4 - 1);
    }

    #[test]
    fn a_stray_index_beyond_the_count_cannot_fake_completion() {
        // idx 0,1 data + idx 2 terminator, count 3. Deliver idx 0, a forged idx
        // 5, and the terminator: three packets received, but idx 1 is missing.
        let unit = [6u8; 1500];
        let pkts = packetize(Seq16(3), 0, false, &[&unit[..]]);
        let mut forged = pkts[1].clone();
        let mut h = header_of(&forged);
        h.pkt_idx = 5;
        h.encode((&mut forged[..HEADER_LEN]).try_into().unwrap());

        let mut r = Reassembler::new();
        assert_eq!(r.push(&pkts[0]), Accept::Buffered);
        assert_eq!(r.push(&forged), Accept::Buffered);
        assert_eq!(r.push(&pkts[2]), Accept::Buffered, "must not complete");
        let Accept::Complete(f) = r.push(&pkts[1]) else {
            panic!("the real fragment completes it")
        };
        assert_eq!(assemble(&mut r, f), unit.to_vec());
    }

    #[test]
    fn releasing_returns_the_storage_and_a_stale_handle_is_harmless() {
        let pkts = packetize(Seq16(1), 0, false, &[&[1u8; 2500][..]]);
        let mut r = Reassembler::new();
        let before = r.pool_available();
        let f = feed(&mut r, &pkts).unwrap();
        assert_eq!(before - r.pool_available(), pkts.len());

        // A second handle to the same slot and generation, as a buggy caller
        // that released twice would hold.
        let stale = FrameRef {
            frame_id: f.frame_id,
            keyframe: f.keyframe,
            qpc_timestamp: f.qpc_timestamp,
            len: f.len,
            slot: f.slot,
            token: f.token,
        };
        r.release(f);
        assert_eq!(r.pool_available(), before, "release returned every slot");

        let mut out = vec![0u8; 4096];
        assert_eq!(r.copy_into(&stale, &mut out), None);
        assert_eq!(r.fragments(&stale).count(), 0);
        r.release(stale);
        assert_eq!(r.pool_available(), before, "no double free");
    }

    #[test]
    fn discard_frees_a_partial_frame_without_logging_it() {
        let pkts = packetize(Seq16(9), 0, false, &[&[1u8; 2500][..]]);
        let mut r = Reassembler::new();
        let before = r.pool_available();
        for pkt in &pkts[..2] {
            r.push(pkt);
        }
        r.discard(Seq16(9));
        assert_eq!(r.pool_available(), before);
        assert!(!r.is_pending(Seq16(9)));
        let mut gone = [Seq16(0); 4];
        assert_eq!(r.drain_abandoned(&mut gone), 0);
    }

    #[test]
    fn a_missing_list_is_bounded_by_the_output_buffer() {
        let pkts = packetize(Seq16(2), 0, false, &[&[1u8; 12 * MAX_PAYLOAD][..]]);
        let mut r = Reassembler::new();
        r.push(pkts.last().unwrap()); // terminator only: twelve holes
        let mut miss = [0u16; 3];
        assert_eq!(r.missing(Seq16(2), &mut miss), 3);
        assert_eq!(miss, [0, 1, 2]);
    }

    #[test]
    fn a_copy_into_a_short_buffer_is_refused() {
        let pkts = packetize(Seq16(1), 0, false, &[&[1u8; 2500][..]]);
        let mut r = Reassembler::new();
        let f = feed(&mut r, &pkts).unwrap();
        let mut out = vec![0u8; 2499];
        assert_eq!(r.copy_into(&f, &mut out), None);
        r.release(f);
    }

    // ---- Jitter buffer ----------------------------------------------------

    fn frame(id: u16) -> FrameRef {
        FrameRef {
            frame_id: Seq16(id),
            keyframe: false,
            qpc_timestamp: 0,
            len: 1,
            slot: id as u32,
            token: 1,
        }
    }

    const MS: u64 = 1_000_000;

    #[test]
    fn releases_in_order_after_the_depth_elapses() {
        let mut j = JitterBuffer::new();
        // Steady 16ms cadence -> negligible jitter, tiny depth.
        for k in 0..4u64 {
            j.push(frame(k as u16), k * 16 * MS).unwrap();
        }
        let mut got = Vec::new();
        // Drain well after the last arrival.
        let now = 100 * MS;
        while let Some(rel) = j.pop(now) {
            assert_eq!(rel.stepped_over, None);
            got.push(rel.frame.frame_id.0);
        }
        assert_eq!(got, vec![0, 1, 2, 3]);
        assert!(j.is_empty());
    }

    #[test]
    fn reordered_arrivals_leave_in_order() {
        let mut j = JitterBuffer::new();
        j.push(frame(2), 0).unwrap();
        j.push(frame(1), 0).unwrap();
        j.push(frame(0), 0).unwrap();
        let mut got = Vec::new();
        while let Some(rel) = j.pop(100 * MS) {
            got.push(rel.frame.frame_id.0);
        }
        assert_eq!(got, vec![0, 1, 2]);
    }

    #[test]
    fn a_frame_that_arrives_after_its_slot_is_handed_back() {
        let mut j = JitterBuffer::new();
        // Release 0 and 1.
        j.push(frame(0), 0).unwrap();
        j.push(frame(1), 16 * MS).unwrap();
        assert_eq!(j.pop(100 * MS).unwrap().frame.frame_id, Seq16(0));
        assert_eq!(j.pop(100 * MS).unwrap().frame.frame_id, Seq16(1));
        // Frame 0 shows up again, far too late: refused so the caller can
        // release its storage, and never queued.
        let late = j.push(frame(0), 120 * MS).unwrap_err();
        assert_eq!(late.frame_id, Seq16(0));
        assert!(j.pop(200 * MS).is_none());
        // A duplicate of something queued is refused the same way.
        j.push(frame(5), 130 * MS).unwrap();
        assert!(j.push(frame(5), 131 * MS).is_err());
    }

    #[test]
    fn a_gap_from_a_lost_frame_is_stepped_over_and_reported() {
        let mut j = JitterBuffer::new();
        j.push(frame(0), 0).unwrap();
        let first = j.pop(100 * MS).unwrap();
        assert_eq!(first.frame.frame_id, Seq16(0));
        assert_eq!(first.stepped_over, None);
        // Frame 1 never arrives; frame 2 does.
        j.push(frame(2), 16 * MS).unwrap();
        let rel = j.pop(100 * MS).unwrap();
        assert_eq!(rel.frame.frame_id, Seq16(2));
        assert_eq!(rel.stepped_over, Some(Seq16(1)), "the skipped id is named");
        // A wider gap names only its oldest id: an abandon covers the rest.
        j.push(frame(6), 32 * MS).unwrap();
        assert_eq!(j.pop(200 * MS).unwrap().stepped_over, Some(Seq16(3)));
    }

    #[test]
    fn depth_stays_small_for_a_steady_stream_and_grows_when_jittery() {
        let mut steady = JitterBuffer::new();
        let mut t = 0u64;
        for k in 0..40u16 {
            t += 16 * MS;
            steady.push(frame(k), t).unwrap();
            steady.pop(t + 50 * MS);
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
            jittery.push(frame(k), t).unwrap();
            jittery.pop(t + 50 * MS);
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
    fn the_depth_cap_follows_the_frame_interval() {
        // At 120 Hz the 8 ms ceiling would be a whole frame; half an interval is
        // the most a cushion may cost.
        let mut j = JitterBuffer::new();
        j.set_frame_interval_ns(8_333_333);
        let mut t = 0u64;
        for k in 0..40u16 {
            t += if k % 2 == 0 { 2 * MS } else { 14 * MS };
            j.push(frame(k), t).unwrap();
            j.pop(t + 50 * MS);
        }
        assert!(j.target_depth_ns() <= 4_166_666);
        assert!(j.target_depth_ns() > 0);
    }

    #[test]
    fn an_overfull_queue_force_releases_rather_than_growing() {
        let mut j = JitterBuffer::new();
        // Many frames all at t=0 with a large depth would otherwise stall; the
        // MAX_QUEUED cap forces the oldest out even before its deadline.
        for k in 0..(MAX_QUEUED as u16 + 1) {
            j.push(frame(k), 0).unwrap();
        }
        // Storage is fixed: one more than it holds is handed straight back.
        assert!(j.push(frame(99), 0).is_err());
        // now == 0, so no frame's depth has elapsed, yet the cap still drains one.
        assert!(j.pop(0).is_some());
        assert_eq!(j.len(), MAX_QUEUED);
    }
}

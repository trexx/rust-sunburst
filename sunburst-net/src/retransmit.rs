// SPDX-License-Identifier: GPL-2.0-or-later

//! The sender's retransmit cache: the last few frames' packets, kept so a
//! NACK can be answered without the encoder.
//!
//! Owned by the send thread alone — it stores each packet as it goes out and
//! serves lookups when a NACK arrives on the same thread — so there is nothing
//! to synchronise. Storage is a fixed ring of whole packets; a frame's packets
//! land in it in `pkt_idx` order because that is the order the packetizer
//! emits them, which is what makes a lookup O(1): the frame's first slot plus
//! the index. Every hit is verified against the slot's own header, so a slot
//! that has since been overwritten by a newer frame is a miss, never a wrong
//! packet.

use sunburst_core::proto::{HEADER_LEN, Header, MAX_PAYLOAD, PacketType, Seq16};

/// Bytes per slot: one whole packet.
const SLOT_BYTES: usize = HEADER_LEN + MAX_PAYLOAD;

/// Frames whose first slot is remembered. Indexed by `frame_id % FRAME_TABLE`,
/// so a frame stays findable until a frame this many later overwrites its
/// entry — long after its packets have left the ring anyway.
const FRAME_TABLE: usize = 64;

/// Default ring: about 5 MB — a few keyframes or a couple of dozen P-frames at
/// 150 Mbps, well past the retransmit horizon of a jitter buffer.
pub const DEFAULT_SLOTS: usize = 4096;

#[derive(Clone, Copy)]
struct FrameEntry {
    frame_id: Seq16,
    /// Ring position of `pkt_idx` 0.
    first: usize,
    /// Packets stored for this frame so far.
    count: u16,
    valid: bool,
}

pub struct RetransmitCache {
    data: Box<[[u8; SLOT_BYTES]]>,
    lens: Box<[u16]>,
    /// Next ring position to write.
    write: usize,
    frames: [FrameEntry; FRAME_TABLE],
    stored: u64,
    hits: u64,
    misses: u64,
}

impl Default for RetransmitCache {
    fn default() -> Self {
        Self::new(DEFAULT_SLOTS)
    }
}

impl RetransmitCache {
    pub fn new(slots: usize) -> RetransmitCache {
        let slots = slots.max(2);
        RetransmitCache {
            data: vec![[0u8; SLOT_BYTES]; slots].into_boxed_slice(),
            lens: vec![0u16; slots].into_boxed_slice(),
            write: 0,
            frames: [FrameEntry {
                frame_id: Seq16(0),
                first: 0,
                count: 0,
                valid: false,
            }; FRAME_TABLE],
            stored: 0,
            hits: 0,
            misses: 0,
        }
    }

    /// Remember an original packet as it is sent. Retransmits must not be
    /// stored again — they are the same bytes, and re-storing would move the
    /// frame's slots. Non-video and malformed packets are ignored.
    pub fn store(&mut self, packet: &[u8]) {
        let Some(header) = Header::decode(packet) else {
            return;
        };
        if header.packet_type != PacketType::Video || packet.len() > SLOT_BYTES {
            return;
        }
        let entry = &mut self.frames[header.frame_id.0 as usize % FRAME_TABLE];
        if !entry.valid || entry.frame_id != header.frame_id {
            *entry = FrameEntry {
                frame_id: header.frame_id,
                first: self.write,
                count: 0,
                valid: true,
            };
        }
        // Sequential is the normal case; a gap (never produced by the
        // packetizer) still stores the packet, and the header check on lookup
        // keeps a skipped index from returning the wrong bytes.
        entry.count = entry.count.max(header.pkt_idx.wrapping_add(1));

        let at = self.write;
        self.data[at][..packet.len()].copy_from_slice(packet);
        self.lens[at] = packet.len() as u16;
        self.write = (self.write + 1) % self.data.len();
        self.stored += 1;
    }

    /// The packet `(frame_id, pkt_idx)` if it is still in the ring.
    pub fn get(&mut self, frame_id: Seq16, pkt_idx: u16) -> Option<&[u8]> {
        let entry = &self.frames[frame_id.0 as usize % FRAME_TABLE];
        if !entry.valid || entry.frame_id != frame_id || pkt_idx >= entry.count {
            self.misses += 1;
            return None;
        }
        let at = (entry.first + pkt_idx as usize) % self.data.len();
        let packet = &self.data[at][..self.lens[at] as usize];
        match Header::decode(packet) {
            Some(h) if h.frame_id == frame_id && h.pkt_idx == pkt_idx => {
                self.hits += 1;
                Some(packet)
            }
            _ => {
                // Overwritten by a later frame: this packet has aged out.
                self.misses += 1;
                None
            }
        }
    }

    /// `(stored, hits, misses)` so far — for the metrics readout.
    pub fn stats(&self) -> (u64, u64, u64) {
        (self.stored, self.hits, self.misses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Packetizer;

    fn frame(id: u16, units: &[&[u8]]) -> Vec<Vec<u8>> {
        let mut p = Packetizer::new();
        let mut out = Vec::new();
        p.begin_frame(Seq16(id), 0, false);
        for u in units {
            p.push_unit(u, |x| out.push(x.to_vec()));
        }
        p.finish_frame(|x| out.push(x.to_vec()));
        out
    }

    #[test]
    fn a_stored_packet_is_found_by_frame_and_index() {
        let mut c = RetransmitCache::new(64);
        let pkts = frame(5, &[&[1u8; 3000]]);
        for p in &pkts {
            c.store(p);
        }
        for (i, p) in pkts.iter().enumerate() {
            assert_eq!(c.get(Seq16(5), i as u16), Some(&p[..]));
        }
        assert_eq!(c.get(Seq16(5), pkts.len() as u16), None, "past the end");
        assert_eq!(c.get(Seq16(6), 0), None, "unknown frame");
        assert_eq!(c.stats(), (4, 4, 2));
    }

    #[test]
    fn frames_interleave_without_confusing_each_other() {
        let mut c = RetransmitCache::new(64);
        let a = frame(1, &[&[1u8; 2000]]);
        let b = frame(2, &[&[2u8; 2000]]);
        for p in a.iter().chain(b.iter()) {
            c.store(p);
        }
        assert_eq!(c.get(Seq16(1), 1), Some(&a[1][..]));
        assert_eq!(c.get(Seq16(2), 1), Some(&b[1][..]));
    }

    #[test]
    fn an_overwritten_slot_is_a_miss_not_a_wrong_packet() {
        // Eight slots; a four-packet frame followed by two more push the first
        // one out of the ring while its frame entry is still valid.
        let mut c = RetransmitCache::new(8);
        let a = frame(1, &[&[1u8; 3000]]);
        for p in &a {
            c.store(p);
        }
        for id in 2..4u16 {
            for p in &frame(id, &[&[9u8; 3000]]) {
                c.store(p);
            }
        }
        assert_eq!(c.get(Seq16(1), 0), None);
        assert_eq!(c.get(Seq16(3), 0).map(|p| p[HEADER_LEN]), Some(9));
    }

    #[test]
    fn a_retransmit_or_a_foreign_packet_does_not_disturb_the_ring() {
        let mut c = RetransmitCache::new(64);
        let pkts = frame(7, &[&[1u8; 3000]]);
        for p in &pkts {
            c.store(p);
        }
        let mut nack = vec![0u8; HEADER_LEN + 4];
        nack[0] = PacketType::Nack as u8;
        c.store(&nack);
        c.store(&[0u8; 3]);
        assert_eq!(c.stats().0, 4, "only video packets are stored");
        assert_eq!(c.get(Seq16(7), 3), Some(&pkts[3][..]));
    }

    #[test]
    fn the_frame_table_wraps_with_the_sequence() {
        let mut c = RetransmitCache::new(256);
        let a = frame(0xFFFF, &[&[1u8; 100]]);
        let b = frame(0x0000, &[&[2u8; 100]]);
        for p in a.iter().chain(b.iter()) {
            c.store(p);
        }
        assert_eq!(c.get(Seq16(0xFFFF), 0), Some(&a[0][..]));
        assert_eq!(c.get(Seq16(0x0000), 0), Some(&b[0][..]));
    }
}

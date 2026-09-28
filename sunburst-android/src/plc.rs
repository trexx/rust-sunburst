// SPDX-License-Identifier: GPL-2.0-or-later

//! Which lost audio packets to conceal — pure, host-tested.
//!
//! Audio rides the same unreliable UDP as video and is never retransmitted, so
//! a lost 5 ms packet used to leave a hole the playback ring filled with
//! silence: an audible click per loss, and a crackle under Wi-Fi loss. Opus has
//! packet-loss concealment for exactly this (`OpusDecoder::conceal`); what the
//! client needs is to notice the gap, from the packet sequence number.
//!
//! Short gaps are concealed packet by packet. A long one (the stream paused, or
//! a burst far past what interpolation can cover) is not: the decoder simply
//! resynchronises on the next packet. A packet older than one already played
//! arrived after its slot was concealed, or is a duplicate, and is dropped.

use sunburst_core::proto::Seq16;

/// Conceal at most this many consecutive lost packets (15 ms at 5 ms frames).
/// Past that, PLC's extrapolation decays into noise and a resync is cleaner.
pub const MAX_CONCEALED: u16 = 3;

/// What to do with an arriving audio packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrival {
    /// Decode it, after concealing this many lost packets before it.
    Play { conceal: u16 },
    /// Late or duplicate: its slot has already played.
    Stale,
}

/// Track the audio packet sequence and classify each arrival.
#[derive(Debug, Default)]
pub struct AudioSequence {
    last: Option<Seq16>,
}

impl AudioSequence {
    pub fn on_packet(&mut self, id: Seq16) -> Arrival {
        let Some(last) = self.last else {
            self.last = Some(id);
            return Arrival::Play { conceal: 0 };
        };
        if !id.is_newer_than(last) {
            return Arrival::Stale;
        }
        self.last = Some(id);
        let lost = id.0.wrapping_sub(last.0).wrapping_sub(1);
        Arrival::Play {
            conceal: if lost <= MAX_CONCEALED { lost } else { 0 },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_order_packets_conceal_nothing() {
        let mut s = AudioSequence::default();
        for id in 0..10 {
            assert_eq!(s.on_packet(Seq16(id)), Arrival::Play { conceal: 0 });
        }
    }

    #[test]
    fn a_short_gap_is_concealed_packet_by_packet() {
        let mut s = AudioSequence::default();
        s.on_packet(Seq16(10));
        assert_eq!(s.on_packet(Seq16(12)), Arrival::Play { conceal: 1 });
        assert_eq!(s.on_packet(Seq16(16)), Arrival::Play { conceal: 3 });
    }

    #[test]
    fn a_long_gap_resyncs_instead() {
        let mut s = AudioSequence::default();
        s.on_packet(Seq16(10));
        assert_eq!(s.on_packet(Seq16(50)), Arrival::Play { conceal: 0 });
    }

    #[test]
    fn late_and_duplicate_packets_are_stale() {
        let mut s = AudioSequence::default();
        s.on_packet(Seq16(10));
        s.on_packet(Seq16(12));
        assert_eq!(
            s.on_packet(Seq16(11)),
            Arrival::Stale,
            "its slot was concealed"
        );
        assert_eq!(s.on_packet(Seq16(12)), Arrival::Stale, "duplicate");
        assert_eq!(s.on_packet(Seq16(13)), Arrival::Play { conceal: 0 });
    }

    #[test]
    fn the_sequence_wraps() {
        let mut s = AudioSequence::default();
        s.on_packet(Seq16(65_534));
        assert_eq!(s.on_packet(Seq16(1)), Arrival::Play { conceal: 2 });
    }
}

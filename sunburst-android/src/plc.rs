// SPDX-License-Identifier: GPL-2.0-or-later

//! Putting audio packets back in order, and which lost ones to conceal — pure,
//! host-tested.
//!
//! Audio rides the same unreliable UDP as video and is never retransmitted, so
//! a lost packet leaves a hole Opus fills with packet-loss concealment
//! (`OpusDecoder::conceal`). Over Wi-Fi, though, most "gaps" are not losses: on
//! the Homatics ~45 packets a second arrived swapped with their neighbour.
//! Concealing at the first gap and then dropping the late packet as stale
//! replaced real audio with an extrapolation twice a second per swap.
//!
//! So a packet that arrives early is held while the one before it may still
//! come: up to [`WINDOW`] − 1 packets. In-order packets pass straight through,
//! so this costs nothing until a gap appears. A hole is given up on only once a
//! packet [`WINDOW`] or more ahead of it arrives. Then a short run of missing
//! packets is concealed packet by packet, and a long one (the stream paused, or
//! a burst past what interpolation can cover) is skipped, so the decoder
//! resynchronises on the next real packet. A packet whose slot has already
//! played, or a duplicate, is dropped.

use sunburst_core::proto::Seq16;
use sunburst_net::MAX_AUDIO_PACKET;

/// Conceal at most this many consecutive lost packets. Past that, PLC's
/// extrapolation decays into noise and a resync is cleaner.
pub const MAX_CONCEALED: u16 = 3;

/// How far ahead of the next expected packet one may arrive before that packet
/// is given up on. At 2.5 ms frames, 3 holds a swap for up to 5 ms.
pub const WINDOW: u16 = 3;

/// A packet this far ahead is a new stream position, not a reorder: drop what
/// is held and start again from it.
pub const RESYNC_AHEAD: u16 = 64;

/// What the caller does next, in order.
#[derive(Debug, PartialEq, Eq)]
pub enum Event<'a> {
    /// Decode and play this Opus packet, which is packet `id`.
    Decode(Seq16, &'a [u8]),
    /// Conceal one lost packet.
    Conceal,
}

/// One held packet.
struct Slot {
    id: Option<Seq16>,
    len: usize,
    data: Box<[u8]>,
}

/// Reorders the audio packet stream within a small window. Allocates once, at
/// construction.
pub struct AudioReorder {
    /// The next packet to play; `None` until the first arrives.
    next: Option<Seq16>,
    /// Packets ahead of `next`, at `id % WINDOW`.
    held: [Slot; WINDOW as usize],
}

impl Default for AudioReorder {
    fn default() -> Self {
        AudioReorder {
            next: None,
            held: std::array::from_fn(|_| Slot {
                id: None,
                len: 0,
                data: vec![0u8; MAX_AUDIO_PACKET].into_boxed_slice(),
            }),
        }
    }
}

impl AudioReorder {
    /// Take one arriving packet. `emit` receives what to play, in order: none
    /// (it was held, late or a duplicate), or one or more events.
    pub fn push(&mut self, id: Seq16, payload: &[u8], mut emit: impl FnMut(Event<'_>)) {
        let Some(mut next) = self.next else {
            self.next = Some(id.next());
            emit(Event::Decode(id, payload));
            return;
        };
        if id != next && !id.is_newer_than(next) {
            return; // its slot has played, or a duplicate of one that did
        }
        if id.0.wrapping_sub(next.0) >= RESYNC_AHEAD {
            for slot in &mut self.held {
                slot.id = None;
            }
            self.next = Some(id.next());
            emit(Event::Decode(id, payload));
            return;
        }

        // Give up on holes until `id` is inside the window. A hole is given up
        // on whole: every packet missing from it up to the next one present
        // (held, or `id`), concealed if the run is short, skipped if long.
        while id.0.wrapping_sub(next.0) >= WINDOW {
            if self.is_held(next) {
                next = self.play_held(next, &mut emit);
                continue;
            }
            let run = self.missing_run(next, id);
            for _ in 0..run {
                if run <= MAX_CONCEALED {
                    emit(Event::Conceal);
                }
                next = next.next();
            }
        }

        if id == next {
            emit(Event::Decode(id, payload));
            next = next.next();
        } else if !self.is_held(id) {
            let slot = &mut self.held[(id.0 % WINDOW) as usize];
            let len = payload.len().min(slot.data.len());
            slot.data[..len].copy_from_slice(&payload[..len]);
            slot.len = len;
            slot.id = Some(id);
        }
        // Whatever was waiting behind `id` (or behind the holes) plays now.
        while self.is_held(next) {
            next = self.play_held(next, &mut emit);
        }
        self.next = Some(next);
    }

    fn is_held(&self, id: Seq16) -> bool {
        self.held[(id.0 % WINDOW) as usize].id == Some(id)
    }

    /// Emit the held packet `id` and free its slot; returns the one after it.
    fn play_held(&mut self, id: Seq16, emit: &mut impl FnMut(Event<'_>)) -> Seq16 {
        let slot = &mut self.held[(id.0 % WINDOW) as usize];
        slot.id = None;
        emit(Event::Decode(id, &slot.data[..slot.len]));
        id.next()
    }

    /// How many packets from `from` are missing before the first one held or
    /// `arrived`.
    fn missing_run(&self, from: Seq16, arrived: Seq16) -> u16 {
        let mut run = 0;
        let mut id = from;
        while id != arrived && !self.is_held(id) {
            run += 1;
            id = id.next();
        }
        run
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Push packets whose payload is their own id, and render the events: `n`
    /// for a decode of packet n, `C` for a conceal.
    fn run(ids: &[u16]) -> Vec<String> {
        let mut r = AudioReorder::default();
        let mut out = Vec::new();
        for &id in ids {
            let payload = id.to_le_bytes();
            r.push(Seq16(id), &payload, |e| {
                out.push(match e {
                    Event::Decode(id, p) => {
                        assert_eq!(id.0, u16::from_le_bytes([p[0], p[1]]), "id/payload");
                        id.0.to_string()
                    }
                    Event::Conceal => "C".into(),
                })
            });
        }
        out
    }

    fn ev(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn in_order_packets_pass_straight_through() {
        assert_eq!(run(&[0, 1, 2, 3]), ev(&["0", "1", "2", "3"]));
    }

    #[test]
    fn a_swapped_pair_is_put_back_in_order_without_concealment() {
        assert_eq!(run(&[0, 2, 1, 3]), ev(&["0", "1", "2", "3"]));
    }

    #[test]
    fn a_packet_two_late_still_makes_it() {
        assert_eq!(run(&[0, 2, 3, 1, 4]), ev(&["0", "1", "2", "3", "4"]));
    }

    #[test]
    fn a_lost_packet_is_concealed_once_the_window_is_exceeded() {
        // 1 never comes: 2 and 3 are held, 4 gives up on it.
        assert_eq!(run(&[0, 2, 3, 4, 5]), ev(&["0", "C", "2", "3", "4", "5"]));
    }

    #[test]
    fn a_late_packet_after_its_slot_was_concealed_is_dropped() {
        assert_eq!(
            run(&[0, 2, 3, 4, 1, 5]),
            ev(&["0", "C", "2", "3", "4", "5"])
        );
    }

    #[test]
    fn a_short_run_of_losses_is_concealed_packet_by_packet() {
        assert_eq!(run(&[0, 4, 5]), ev(&["0", "C", "C", "C", "4", "5"]));
    }

    #[test]
    fn a_long_run_is_skipped_for_a_resync() {
        assert_eq!(run(&[0, 10, 11]), ev(&["0", "10", "11"]));
    }

    #[test]
    fn a_far_jump_drops_what_is_held_and_starts_again() {
        assert_eq!(run(&[0, 2, 500, 501]), ev(&["0", "500", "501"]));
    }

    #[test]
    fn duplicates_play_once() {
        assert_eq!(run(&[0, 1, 1, 2, 2]), ev(&["0", "1", "2"]));
        assert_eq!(run(&[0, 2, 2, 1]), ev(&["0", "1", "2"]));
    }

    #[test]
    fn the_sequence_wraps() {
        assert_eq!(
            run(&[65_534, 0, 65_535, 1]),
            ev(&["65534", "65535", "0", "1"])
        );
    }
}

// SPDX-License-Identifier: GPL-2.0-or-later

//! Server → client rumble.
//!
//! Carried unreliably, deliberately. A superseded rumble level is worthless, so
//! latest-wins beats guaranteed delivery — retransmitting a level the game has
//! already moved past is strictly worse than dropping it.
//!
//! That choice creates its own failure, and [`RumbleTracker`] is the answer to
//! it: if the final zero-level packet is lost, an unreliable channel leaves the
//! motor running until the battery dies. The client stops any motor it has not
//! heard about within [`RUMBLE_TIMEOUT_MS`], and the server repeats a non-zero
//! level every [`RUMBLE_REPEAT_MS`].

use super::input::MAX_PADS;

/// Encoded rumble body length: pad, four motors (low, high, two triggers),
/// sequence. The two trigger motors are the Xbox impulse triggers; a pad without
/// them simply leaves them zero, so the wire size is fixed regardless of family.
pub const RUMBLE_BODY_LEN: usize = 10;

/// Stop a motor that has gone unheard this long.
pub const RUMBLE_TIMEOUT_MS: u32 = 200;

/// Repeat a non-zero level this often, so a lost packet self-heals.
pub const RUMBLE_REPEAT_MS: u32 = 100;

/// One pad's motor levels.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rumble {
    pub pad_index: u8,
    /// Large, low-frequency motor.
    pub motor_low: u16,
    /// Small, high-frequency motor.
    pub motor_high: u16,
    /// Left impulse-trigger motor (Xbox One/Series). Zero on pads without it.
    pub trigger_left: u16,
    /// Right impulse-trigger motor (Xbox One/Series). Zero on pads without it.
    pub trigger_right: u16,
    /// Wraps. Compared modularly so reordering cannot strand a motor at a stale
    /// level — the failure you feel in your hands rather than see in a log.
    pub seq: u8,
}

impl Rumble {
    pub const fn is_silent(&self) -> bool {
        self.motor_low == 0
            && self.motor_high == 0
            && self.trigger_left == 0
            && self.trigger_right == 0
    }

    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < RUMBLE_BODY_LEN {
            return None;
        }
        out[0] = self.pad_index;
        out[1..3].copy_from_slice(&self.motor_low.to_le_bytes());
        out[3..5].copy_from_slice(&self.motor_high.to_le_bytes());
        out[5..7].copy_from_slice(&self.trigger_left.to_le_bytes());
        out[7..9].copy_from_slice(&self.trigger_right.to_le_bytes());
        out[9] = self.seq;
        Some(RUMBLE_BODY_LEN)
    }

    pub fn decode(buf: &[u8]) -> Option<Rumble> {
        if buf.len() < RUMBLE_BODY_LEN {
            return None;
        }
        let pad_index = buf[0];
        if pad_index >= MAX_PADS {
            return None;
        }
        Some(Rumble {
            pad_index,
            motor_low: u16::from_le_bytes([buf[1], buf[2]]),
            motor_high: u16::from_le_bytes([buf[3], buf[4]]),
            trigger_left: u16::from_le_bytes([buf[5], buf[6]]),
            trigger_right: u16::from_le_bytes([buf[7], buf[8]]),
            seq: buf[9],
        })
    }
}

/// Client-side state: applies fresh levels, ignores stale ones, and stops
/// motors that have gone quiet.
///
/// Time is passed in rather than read, so this is testable without sleeping and
/// costs no clock indirection on the caller's path.
#[derive(Debug, Default)]
pub struct RumbleTracker {
    pads: [PadState; MAX_PADS as usize],
}

#[derive(Clone, Copy, Debug, Default)]
struct PadState {
    seq: u8,
    started: bool,
    last_heard_ms: u32,
    current: Rumble,
}

impl RumbleTracker {
    pub fn new() -> RumbleTracker {
        RumbleTracker::default()
    }

    /// Apply an arriving packet. Returns the level to set, or `None` if the
    /// packet was stale and should be ignored.
    pub fn apply(&mut self, r: Rumble, now_ms: u32) -> Option<Rumble> {
        let pad = &mut self.pads[r.pad_index as usize];

        if pad.started {
            // Modular comparison: `seq` is a byte and wraps every 256 packets,
            // which at the 100ms repeat interval is under half a minute.
            let forward = r.seq.wrapping_sub(pad.seq);
            if forward == 0 || forward >= 0x80 {
                // A repeat of the current level still counts as being heard —
                // otherwise the server's own keepalive would let the timeout
                // below fire during sustained rumble.
                if forward == 0 {
                    pad.last_heard_ms = now_ms;
                }
                return None;
            }
        }

        pad.started = true;
        pad.seq = r.seq;
        pad.last_heard_ms = now_ms;
        pad.current = r;
        Some(r)
    }

    /// Motors to silence because nothing has been heard for
    /// [`RUMBLE_TIMEOUT_MS`].
    ///
    /// Call periodically. Returns each pad once: after reporting, the pad is
    /// considered silent and will not be reported again until it rumbles anew.
    pub fn expired(&mut self, now_ms: u32) -> impl Iterator<Item = u8> + use<> {
        let mut stopped = [false; MAX_PADS as usize];
        for (i, pad) in self.pads.iter_mut().enumerate() {
            if !pad.started || pad.current.is_silent() {
                continue;
            }
            if now_ms.wrapping_sub(pad.last_heard_ms) >= RUMBLE_TIMEOUT_MS {
                pad.current = Rumble {
                    pad_index: i as u8,
                    ..Default::default()
                };
                stopped[i] = true;
            }
        }
        (0..MAX_PADS).filter(move |i| stopped[*i as usize])
    }

    /// The level currently applied to a pad.
    pub fn current(&self, pad_index: u8) -> Rumble {
        self.pads[pad_index as usize].current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rumble(pad_index: u8, level: u16, seq: u8) -> Rumble {
        Rumble {
            pad_index,
            motor_low: level,
            motor_high: level / 2,
            // Distinct trigger levels so the round trip and is_silent cover them.
            trigger_left: level / 4,
            trigger_right: level / 8,
            seq,
        }
    }

    #[test]
    fn round_trips() {
        let r = rumble(3, 40000, 200);
        let mut buf = [0u8; RUMBLE_BODY_LEN];
        let n = r.encode(&mut buf).unwrap();
        assert_eq!(n, RUMBLE_BODY_LEN);
        assert_eq!(Rumble::decode(&buf[..n]), Some(r));
    }

    #[test]
    fn trigger_motors_round_trip_and_count_as_active() {
        // A pad rumbling only its impulse triggers must survive the wire and not
        // read as silent, or the tracker's timeout would cut trigger rumble off.
        let r = Rumble {
            pad_index: 1,
            motor_low: 0,
            motor_high: 0,
            trigger_left: 0xABCD,
            trigger_right: 0x1234,
            seq: 9,
        };
        assert!(!r.is_silent());
        let mut buf = [0u8; RUMBLE_BODY_LEN];
        let n = r.encode(&mut buf).unwrap();
        assert_eq!(Rumble::decode(&buf[..n]), Some(r));
    }

    #[test]
    fn refuses_a_pad_the_client_does_not_have() {
        let mut buf = [0u8; RUMBLE_BODY_LEN];
        rumble(0, 1, 1).encode(&mut buf).unwrap();
        buf[0] = MAX_PADS;
        assert_eq!(Rumble::decode(&buf), None);
    }

    #[test]
    fn refuses_a_truncated_body() {
        for len in 0..RUMBLE_BODY_LEN {
            assert_eq!(Rumble::decode(&[0u8; RUMBLE_BODY_LEN][..len]), None);
        }
    }

    #[test]
    fn newer_levels_are_applied() {
        let mut t = RumbleTracker::new();
        assert!(t.apply(rumble(0, 100, 1), 0).is_some());
        assert!(t.apply(rumble(0, 200, 2), 10).is_some());
        assert_eq!(t.current(0).motor_low, 200);
    }

    #[test]
    fn reordered_packets_do_not_strand_a_motor() {
        // The failure this guards: seq 2 arrives, then the delayed seq 1, and
        // the motor is left at the older level indefinitely.
        let mut t = RumbleTracker::new();
        assert!(t.apply(rumble(0, 100, 1), 0).is_some());
        assert!(t.apply(rumble(0, 0, 3), 10).is_some());
        assert!(
            t.apply(rumble(0, 500, 2), 20).is_none(),
            "a late packet must not overwrite a newer level"
        );
        assert_eq!(t.current(0).motor_low, 0);
    }

    #[test]
    fn the_sequence_wraps_without_stalling() {
        // A byte wraps every 256 packets, well under a minute at the repeat
        // interval, so this is ordinary operation rather than an edge case.
        let mut t = RumbleTracker::new();
        assert!(t.apply(rumble(0, 100, 254), 0).is_some());
        assert!(t.apply(rumble(0, 110, 255), 10).is_some());
        assert!(t.apply(rumble(0, 120, 0), 20).is_some());
        assert!(t.apply(rumble(0, 130, 1), 30).is_some());
        assert_eq!(t.current(0).motor_low, 130);
        assert!(
            t.apply(rumble(0, 999, 250), 40).is_none(),
            "a pre-wrap packet is still old"
        );
    }

    #[test]
    fn a_motor_left_running_is_stopped() {
        // The whole reason the timeout exists: the final zero-level packet is
        // lost and nothing else arrives.
        let mut t = RumbleTracker::new();
        t.apply(rumble(0, 60000, 1), 0);

        assert_eq!(t.expired(RUMBLE_TIMEOUT_MS - 1).count(), 0);

        let stopped: Vec<_> = t.expired(RUMBLE_TIMEOUT_MS).collect();
        assert_eq!(stopped, vec![0]);
        assert!(t.current(0).is_silent());

        // Reported once, not every tick thereafter.
        assert_eq!(t.expired(RUMBLE_TIMEOUT_MS + 1000).count(), 0);
    }

    #[test]
    fn a_repeated_level_keeps_the_motor_alive() {
        // The server repeats a non-zero level every RUMBLE_REPEAT_MS with the
        // same sequence number. If that did not count as being heard, sustained
        // rumble would cut out after RUMBLE_TIMEOUT_MS.
        let mut t = RumbleTracker::new();
        t.apply(rumble(0, 60000, 1), 0);
        for tick in 1..10 {
            let now = tick * RUMBLE_REPEAT_MS;
            assert!(
                t.apply(rumble(0, 60000, 1), now).is_none(),
                "a repeat is not a new level"
            );
            assert_eq!(t.expired(now).count(), 0, "but it does keep the pad alive");
        }
        assert!(!t.current(0).is_silent());
    }

    #[test]
    fn a_silent_motor_is_not_repeatedly_stopped() {
        let mut t = RumbleTracker::new();
        t.apply(rumble(0, 0, 1), 0);
        assert_eq!(t.expired(RUMBLE_TIMEOUT_MS * 10).count(), 0);
    }

    #[test]
    fn pads_are_tracked_independently() {
        let mut t = RumbleTracker::new();
        t.apply(rumble(0, 100, 1), 0);
        t.apply(rumble(1, 200, 1), 150);

        // Pad 0 times out first; pad 1 is still fresh.
        let stopped: Vec<_> = t.expired(RUMBLE_TIMEOUT_MS).collect();
        assert_eq!(stopped, vec![0]);
        assert!(!t.current(1).is_silent());

        let stopped: Vec<_> = t.expired(150 + RUMBLE_TIMEOUT_MS).collect();
        assert_eq!(stopped, vec![1]);
    }
}

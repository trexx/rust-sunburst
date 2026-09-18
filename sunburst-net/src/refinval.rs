// SPDX-License-Identifier: GPL-2.0-or-later

//! Reference-frame invalidation, and the intra-refresh schedule beside it.
//!
//! When the client abandons a frame, every frame the encoder has predicted
//! from it since is undecodable too. The server answers by invalidating that
//! whole range (`NvEncInvalidateRefFrames`, once per frame) so the next frame
//! predicts from an older, good reference — and falls back to a keyframe only
//! when nothing good is left in the DPB.
//!
//! Two state machines, one trait, per CLAUDE.md: HEVC's DPB and AV1's eight
//! explicit slots are different enough that sharing one implementation would
//! be the kind of "works on the Shield, corrupts on the Homatics" bug that is
//! very hard to attribute. Today they differ in depth; the separation is what
//! lets AV1's slot model diverge further without touching HEVC.
//!
//! Pure logic, no NVENC: the machine tracks `(frame_id, timestamp)` pairs and
//! answers with a range of frame ids; the pipeline looks each one's timestamp
//! up and makes the calls.

use sunburst_core::proto::Seq16;

/// What to do about an abandoned frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Recovery {
    /// Already covered by an earlier recovery, or older than anything still
    /// referenced. Do nothing.
    Nothing,
    /// Invalidate every frame from `from` through `to` inclusive (walk with
    /// [`Seq16::next`], look each timestamp up with [`RefState::timestamp_of`]).
    Invalidate { from: Seq16, to: Seq16 },
    /// Nothing decodable would be left to reference: force a keyframe.
    ForceIdr,
}

pub trait RefState {
    /// A frame went out. `keyframe` resets the machine: everything before it
    /// is no longer a reference.
    fn on_encoded(&mut self, frame_id: Seq16, timestamp: u64, keyframe: bool);

    /// The client abandoned `frame_id`.
    fn on_abandoned(&mut self, frame_id: Seq16) -> Recovery;

    /// The timestamp `on_encoded` recorded for a frame still tracked.
    fn timestamp_of(&self, frame_id: Seq16) -> Option<u64>;
}

/// Frames remembered. Only the DPB's worth matter for the decision, but a
/// longer memory answers "older than anything tracked" precisely.
const TRACK: usize = 64;

#[derive(Clone, Copy, Default)]
struct Encoded {
    frame_id: Seq16,
    timestamp: u64,
    valid: bool,
}

/// The shared bookkeeping; each codec wraps it with its own depth rule.
struct Window {
    ring: [Encoded; TRACK],
    next: usize,
    newest: Option<Seq16>,
    /// Frames at or before this have been dealt with: invalidated, or
    /// superseded by a keyframe. An abandon here is old news.
    recovered_through: Option<Seq16>,
}

impl Window {
    fn new() -> Window {
        Window {
            ring: [Encoded::default(); TRACK],
            next: 0,
            newest: None,
            recovered_through: None,
        }
    }

    fn on_encoded(&mut self, frame_id: Seq16, timestamp: u64, keyframe: bool) {
        self.ring[self.next] = Encoded {
            frame_id,
            timestamp,
            valid: true,
        };
        self.next = (self.next + 1) % TRACK;
        self.newest = Some(frame_id);
        if keyframe {
            // A keyframe references nothing, so no abandon before it can
            // matter any more.
            self.recovered_through = Some(frame_id);
        }
    }

    fn timestamp_of(&self, frame_id: Seq16) -> Option<u64> {
        self.ring
            .iter()
            .find(|e| e.valid && e.frame_id == frame_id)
            .map(|e| e.timestamp)
    }

    /// `depth` is how many reconstructed frames the DPB keeps besides the one
    /// being encoded: the frame before `frame_id` is still a reference only if
    /// it is within that many of the newest.
    fn on_abandoned(&mut self, frame_id: Seq16, depth: u16) -> Recovery {
        let Some(newest) = self.newest else {
            return Recovery::Nothing; // nothing encoded yet
        };
        if let Some(through) = self.recovered_through
            && !frame_id.is_newer_than(through)
        {
            return Recovery::Nothing;
        }
        if self.timestamp_of(frame_id).is_none() {
            // Not tracked: too old, or never encoded. Either way nothing
            // reliable is left to predict from.
            self.recovered_through = Some(newest);
            return Recovery::ForceIdr;
        }
        let since = newest.distance_from(frame_id);
        self.recovered_through = Some(newest);
        // The reference the next frame will fall back to is the one before
        // the abandoned frame. It has aged out of the DPB once `since + 1`
        // frames have been reconstructed after it.
        if since < 0 || since as u32 + 1 >= depth as u32 {
            return Recovery::ForceIdr;
        }
        Recovery::Invalidate {
            from: frame_id,
            to: newest,
        }
    }
}

/// HEVC: a DPB of `dpb_depth` reconstructed frames (`maxNumRefFramesInDPB`).
pub struct HevcRefState {
    window: Window,
    dpb_depth: u16,
}

impl HevcRefState {
    /// `dpb_depth` must match what the encoder was initialised with.
    pub fn new(dpb_depth: u16) -> HevcRefState {
        HevcRefState {
            window: Window::new(),
            dpb_depth: dpb_depth.max(2),
        }
    }
}

impl RefState for HevcRefState {
    fn on_encoded(&mut self, frame_id: Seq16, timestamp: u64, keyframe: bool) {
        self.window.on_encoded(frame_id, timestamp, keyframe);
    }

    fn on_abandoned(&mut self, frame_id: Seq16) -> Recovery {
        self.window.on_abandoned(frame_id, self.dpb_depth)
    }

    fn timestamp_of(&self, frame_id: Seq16) -> Option<u64> {
        self.window.timestamp_of(frame_id)
    }
}

/// AV1's reference model: eight slots, one of which is being written, so seven
/// reconstructed frames are the most that can ever be referenced.
pub const AV1_REF_SLOTS: u16 = 8;

/// AV1: the explicit eight-slot model.
pub struct Av1RefState {
    window: Window,
}

impl Default for Av1RefState {
    fn default() -> Self {
        Self::new()
    }
}

impl Av1RefState {
    pub fn new() -> Av1RefState {
        Av1RefState {
            window: Window::new(),
        }
    }
}

impl RefState for Av1RefState {
    fn on_encoded(&mut self, frame_id: Seq16, timestamp: u64, keyframe: bool) {
        self.window.on_encoded(frame_id, timestamp, keyframe);
    }

    fn on_abandoned(&mut self, frame_id: Seq16) -> Recovery {
        self.window.on_abandoned(frame_id, AV1_REF_SLOTS)
    }

    fn timestamp_of(&self, frame_id: Seq16) -> Option<u64> {
        self.window.timestamp_of(frame_id)
    }
}

/// The periodic intra-refresh schedule the encoder was configured with, so the
/// packetizer can flag the frames inside a refresh wave.
///
/// `period` frames between waves, each `cnt` frames long, counted from the
/// last keyframe. Whether NVENC phases its wave exactly like this is
/// box-to-validate; the flag is informational to the client either way.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IntraRefresh {
    pub period: u32,
    pub cnt: u32,
}

impl IntraRefresh {
    /// Whether frame number `frames_since_idr` (0 = the keyframe itself) is
    /// inside a refresh wave.
    pub fn active(&self, frames_since_idr: u32) -> bool {
        if self.period == 0 || frames_since_idr == 0 {
            return false;
        }
        frames_since_idr % self.period < self.cnt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded<S: RefState>(s: &mut S, from: u16, to: u16) {
        for id in from..=to {
            s.on_encoded(Seq16(id), 1000 + id as u64, id == from);
        }
    }

    #[test]
    fn an_abandon_inside_the_dpb_invalidates_through_the_newest() {
        let mut s = HevcRefState::new(8);
        encoded(&mut s, 0, 20);
        assert_eq!(
            s.on_abandoned(Seq16(18)),
            Recovery::Invalidate {
                from: Seq16(18),
                to: Seq16(20)
            }
        );
        assert_eq!(s.timestamp_of(Seq16(18)), Some(1018));
        assert_eq!(s.timestamp_of(Seq16(20)), Some(1020));
    }

    #[test]
    fn an_abandon_whose_predecessor_left_the_dpb_forces_a_keyframe() {
        let mut s = HevcRefState::new(8);
        encoded(&mut s, 0, 20);
        // Frame 13's fallback reference is 12; frames 13..=20 are eight
        // reconstructions after it, so it is gone from an 8-deep DPB.
        assert_eq!(s.on_abandoned(Seq16(13)), Recovery::ForceIdr);
        // One frame younger and the reference (13) is still there.
        let mut s = HevcRefState::new(8);
        encoded(&mut s, 0, 20);
        assert_eq!(
            s.on_abandoned(Seq16(14)),
            Recovery::Invalidate {
                from: Seq16(14),
                to: Seq16(20)
            }
        );
    }

    #[test]
    fn a_second_abandon_at_or_before_the_recovery_point_is_nothing() {
        let mut s = HevcRefState::new(8);
        encoded(&mut s, 0, 20);
        assert!(matches!(
            s.on_abandoned(Seq16(19)),
            Recovery::Invalidate { .. }
        ));
        assert_eq!(s.on_abandoned(Seq16(19)), Recovery::Nothing);
        assert_eq!(s.on_abandoned(Seq16(20)), Recovery::Nothing);
        assert_eq!(s.on_abandoned(Seq16(17)), Recovery::Nothing);
        // A frame encoded after the recovery is a fresh question.
        s.on_encoded(Seq16(21), 1021, false);
        assert_eq!(
            s.on_abandoned(Seq16(21)),
            Recovery::Invalidate {
                from: Seq16(21),
                to: Seq16(21)
            }
        );
    }

    #[test]
    fn a_keyframe_makes_older_abandons_irrelevant() {
        let mut s = HevcRefState::new(8);
        encoded(&mut s, 0, 10);
        s.on_encoded(Seq16(11), 1011, true);
        assert_eq!(s.on_abandoned(Seq16(10)), Recovery::Nothing);
        assert_eq!(s.on_abandoned(Seq16(11)), Recovery::Nothing);
    }

    #[test]
    fn an_untracked_frame_forces_a_keyframe() {
        let mut s = HevcRefState::new(8);
        assert_eq!(
            s.on_abandoned(Seq16(3)),
            Recovery::Nothing,
            "nothing encoded yet"
        );
        encoded(&mut s, 100, 300); // 201 frames; the ring keeps 64
        assert_eq!(s.on_abandoned(Seq16(150)), Recovery::ForceIdr);
    }

    #[test]
    fn the_range_walks_across_the_sequence_wrap() {
        let mut s = HevcRefState::new(8);
        for (i, id) in (0xFFFDu16..=0xFFFF).chain(0..=2).enumerate() {
            s.on_encoded(Seq16(id), i as u64, i == 0);
        }
        assert_eq!(
            s.on_abandoned(Seq16(0xFFFF)),
            Recovery::Invalidate {
                from: Seq16(0xFFFF),
                to: Seq16(2)
            }
        );
        let mut walked = Vec::new();
        let mut id = Seq16(0xFFFF);
        loop {
            walked.push(s.timestamp_of(id).unwrap());
            if id == Seq16(2) {
                break;
            }
            id = id.next();
        }
        assert_eq!(walked, vec![2, 3, 4, 5]);
    }

    #[test]
    fn av1_uses_its_slot_count_as_the_depth() {
        let mut s = Av1RefState::new();
        encoded(&mut s, 0, 20);
        // Seven usable references: 13's predecessor 12 is eight back — gone.
        assert_eq!(s.on_abandoned(Seq16(13)), Recovery::ForceIdr);
        let mut s = Av1RefState::new();
        encoded(&mut s, 0, 20);
        assert_eq!(
            s.on_abandoned(Seq16(14)),
            Recovery::Invalidate {
                from: Seq16(14),
                to: Seq16(20)
            }
        );
    }

    #[test]
    fn hevc_depth_is_configurable_and_av1_is_not() {
        let mut deep = HevcRefState::new(16);
        encoded(&mut deep, 0, 20);
        assert!(matches!(
            deep.on_abandoned(Seq16(8)),
            Recovery::Invalidate { .. }
        ));
        let mut shallow = HevcRefState::new(4);
        encoded(&mut shallow, 0, 20);
        assert_eq!(
            shallow.on_abandoned(Seq16(18)),
            Recovery::Invalidate {
                from: Seq16(18),
                to: Seq16(20)
            }
        );
        assert_eq!(
            shallow.on_abandoned(Seq16(17)),
            Recovery::Nothing,
            "covered"
        );
        let mut shallow = HevcRefState::new(4);
        encoded(&mut shallow, 0, 20);
        assert_eq!(shallow.on_abandoned(Seq16(17)), Recovery::ForceIdr);
    }

    #[test]
    fn intra_refresh_waves_are_periodic_and_never_on_the_keyframe() {
        let ir = IntraRefresh {
            period: 60,
            cnt: 15,
        };
        assert!(!ir.active(0));
        assert!(ir.active(1));
        assert!(ir.active(14));
        assert!(!ir.active(15));
        assert!(!ir.active(59));
        assert!(ir.active(60));
        assert!(ir.active(74));
        assert!(!ir.active(75));
        assert!(!IntraRefresh { period: 0, cnt: 0 }.active(5));
    }
}

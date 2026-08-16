// SPDX-License-Identifier: GPL-2.0-or-later

//! Pipeline stage identifiers, shared by both ends of the link.
//!
//! One enum rather than one per side, because timestamps cross the wire and the
//! client reports against the same ids the server does.
//!
//! **The order is the pipeline order, and the drain thread depends on it.** A
//! stage's duration is measured against its immediate predecessor here, so
//! inserting a variant in the wrong place silently redefines two measurements.

/// Number of stages. Sized off the enum so adding a variant cannot leave an
/// array behind.
pub const STAGE_COUNT: usize = Stage::Present as usize + 1;

/// A point in the frame pipeline. Recording one marks that stage *completing*
/// for a given frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Stage {
    // Server.
    CaptureAcquire = 0,
    ColorConvert = 1,
    EncodeSubmit = 2,
    EncodeUnitOut = 3,
    Packetize = 4,
    Send = 5,

    // Client. `Recv` opens a new chain: the gap between `Send` and `Recv`
    // spans two machines whose clocks share no epoch, so it is wire time and
    // cannot be derived by subtraction here.
    Recv = 6,
    JitterOut = 7,
    DecodeSubmit = 8,
    DecodeOut = 9,
    Present = 10,
}

impl Stage {
    /// Short label for the readout table.
    pub const fn name(self) -> &'static str {
        match self {
            Stage::CaptureAcquire => "capture",
            Stage::ColorConvert => "convert",
            Stage::EncodeSubmit => "enc-submit",
            Stage::EncodeUnitOut => "enc-unit",
            Stage::Packetize => "packetize",
            Stage::Send => "send",
            Stage::Recv => "recv",
            Stage::JitterOut => "jitter",
            Stage::DecodeSubmit => "dec-submit",
            Stage::DecodeOut => "dec-out",
            Stage::Present => "present",
        }
    }

    /// Whether this stage begins a new timing chain, so no duration is derived
    /// from its predecessor.
    ///
    /// True for the first stage on each machine. Everything else measures
    /// against the stage before it.
    pub const fn starts_chain(self) -> bool {
        matches!(self, Stage::CaptureAcquire | Stage::Recv)
    }

    /// Recover a stage from its wire/ring representation.
    pub const fn from_u8(v: u8) -> Option<Stage> {
        Some(match v {
            0 => Stage::CaptureAcquire,
            1 => Stage::ColorConvert,
            2 => Stage::EncodeSubmit,
            3 => Stage::EncodeUnitOut,
            4 => Stage::Packetize,
            5 => Stage::Send,
            6 => Stage::Recv,
            7 => Stage::JitterOut,
            8 => Stage::DecodeSubmit,
            9 => Stage::DecodeOut,
            10 => Stage::Present,
            _ => return None,
        })
    }

    /// Every stage, in pipeline order.
    pub const ALL: [Stage; STAGE_COUNT] = [
        Stage::CaptureAcquire,
        Stage::ColorConvert,
        Stage::EncodeSubmit,
        Stage::EncodeUnitOut,
        Stage::Packetize,
        Stage::Send,
        Stage::Recv,
        Stage::JitterOut,
        Stage::DecodeSubmit,
        Stage::DecodeOut,
        Stage::Present,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_is_complete_and_in_order() {
        assert_eq!(Stage::ALL.len(), STAGE_COUNT);
        for (i, s) in Stage::ALL.iter().enumerate() {
            assert_eq!(*s as usize, i, "{} is out of order", s.name());
        }
    }

    #[test]
    fn from_u8_round_trips_and_rejects_the_rest() {
        for s in Stage::ALL {
            assert_eq!(Stage::from_u8(s as u8), Some(s));
        }
        assert_eq!(Stage::from_u8(STAGE_COUNT as u8), None);
        assert_eq!(Stage::from_u8(u8::MAX), None);
    }

    #[test]
    fn exactly_two_chains_start() {
        // One per machine. A third would mean a stage stopped being measured
        // without anyone noticing.
        let starts: Vec<_> = Stage::ALL.iter().filter(|s| s.starts_chain()).collect();
        assert_eq!(
            starts.len(),
            2,
            "expected one chain per machine: {starts:?}"
        );
    }

    #[test]
    fn names_are_unique() {
        let mut names: Vec<_> = Stage::ALL.iter().map(|s| s.name()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate stage name");
    }
}

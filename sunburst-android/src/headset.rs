// SPDX-License-Identifier: GPL-2.0-or-later

//! The shared headset-enablement gate.
//!
//! An Xbox headset's audio sub-device does both directions at once — playback to
//! the headphones and capture from the mic — over the same isochronous-USB
//! bandwidth on the shared adapter, so the cap of [`MAX_HEADSETS`] concurrent
//! headsets is on *enabled headsets*, not per direction. Both the playback fork
//! and the mic capture must therefore consult **one** gate; two separate caps
//! could enable four.
//!
//! This is the pure decision — the ≤2 cap and which pads are enabled — split out
//! from the Android-gated `crate::audio::PadHeadsets` that owns the bridge I/O, so
//! it is host-tested (like [`crate::pad`] and [`crate::mic`]).

const MAX_PADS: usize = sunburst_core::proto::input::MAX_PADS as usize;

/// Concurrent enabled headsets, capped by the shared adapter's iso bandwidth.
pub const MAX_HEADSETS: usize = 2;

/// Tracks which pads have their headset audio sub-device enabled, under the
/// [`MAX_HEADSETS`] cap. Neither direction ever *disables* a headset — a pad
/// going away ([`Self::forget`]) is what frees a slot — so the only contended
/// decision is which pads get the scarce enabled slots.
pub struct HeadsetGate {
    enabled: [bool; MAX_PADS],
    active: usize,
}

impl Default for HeadsetGate {
    fn default() -> HeadsetGate {
        HeadsetGate::new()
    }
}

impl HeadsetGate {
    pub fn new() -> HeadsetGate {
        HeadsetGate {
            enabled: [false; MAX_PADS],
            active: 0,
        }
    }

    /// Whether `pad`'s headset is currently marked enabled.
    pub fn is_enabled(&self, pad: usize) -> bool {
        pad < MAX_PADS && self.enabled[pad]
    }

    /// Reserve `pad`'s enabled slot. Returns whether `pad` is enabled afterwards:
    /// `true` if it already was (idempotent, no extra slot) or a free slot was
    /// taken; `false` if the [`MAX_HEADSETS`] cap is full and `pad` was not already
    /// enabled. The caller performs the actual bridge enable on a fresh reservation
    /// (distinguished by a preceding [`Self::is_enabled`] check) and calls
    /// [`Self::forget`] if that bridge call fails.
    pub fn reserve(&mut self, pad: usize) -> bool {
        if pad >= MAX_PADS {
            return false;
        }
        if self.enabled[pad] {
            return true;
        }
        if self.active >= MAX_HEADSETS {
            return false;
        }
        self.enabled[pad] = true;
        self.active += 1;
        true
    }

    /// Release `pad`'s slot if it held one. Idempotent — a pad that was not enabled
    /// is a no-op, so both the playback and capture paths may call it freely when a
    /// headset goes away.
    pub fn forget(&mut self, pad: usize) {
        if pad < MAX_PADS && self.enabled[pad] {
            self.enabled[pad] = false;
            self.active -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserves_up_to_the_cap_then_refuses() {
        let mut g = HeadsetGate::new();
        assert!(g.reserve(0));
        assert!(g.reserve(1));
        assert!(!g.reserve(2), "the third headset is over the cap");
        assert!(g.is_enabled(0) && g.is_enabled(1) && !g.is_enabled(2));
    }

    #[test]
    fn forget_frees_a_slot_for_the_next() {
        let mut g = HeadsetGate::new();
        assert!(g.reserve(0));
        assert!(g.reserve(1));
        assert!(!g.reserve(3));
        g.forget(0);
        assert!(!g.is_enabled(0));
        assert!(g.reserve(3), "a freed slot admits the next headset");
        assert!(g.is_enabled(3));
    }

    #[test]
    fn reserve_is_idempotent() {
        let mut g = HeadsetGate::new();
        assert!(g.reserve(0));
        assert!(g.reserve(0), "already enabled");
        assert!(g.reserve(0));
        // Only one slot was consumed, so a second distinct pad still fits.
        assert!(g.reserve(1));
        assert!(!g.reserve(2), "still only two slots");
    }

    #[test]
    fn forget_of_an_unenabled_pad_is_a_noop() {
        let mut g = HeadsetGate::new();
        g.forget(0); // never enabled
        g.forget(2);
        assert!(g.reserve(0));
        assert!(g.reserve(1));
        assert!(!g.reserve(2), "the cap is intact — no phantom free slot");
    }
}

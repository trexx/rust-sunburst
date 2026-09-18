// SPDX-License-Identifier: GPL-2.0-or-later

//! `pad_type` → the controller the server emulates.
//!
//! A client announces its physical pad's type in `PadConnected.pad_type`; the
//! server emulates the same family so a physical DualSense presents as a virtual
//! DualSense, an Xbox pad as an Xbox pad. This maps that byte to a vendored
//! profile and builds its [`PadSession`].
//!
//! The priority families are embedded via `include_str!` (as the golden tests
//! embed theirs), so the injector has no runtime file dependency; more families
//! are added by embedding their JSON here. The codes are pinned in `PROTOCOL.md`.

use super::profile::Profile;
use super::session::{PadSession, SessionError};

/// The controller family a virtual pad emulates. The wire value is
/// `PadConnected.pad_type`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum PadType {
    /// The universal, most-compatible default.
    Xbox360 = 0,
    XboxSeriesXS = 1,
    DualShock4 = 2,
    DualSense = 3,
    SwitchPro = 4,
}

impl PadType {
    pub const fn from_u8(v: u8) -> Option<PadType> {
        Some(match v {
            0 => PadType::Xbox360,
            1 => PadType::XboxSeriesXS,
            2 => PadType::DualShock4,
            3 => PadType::DualSense,
            4 => PadType::SwitchPro,
            _ => return None,
        })
    }

    /// The vendored profile JSON this family emulates.
    fn profile_json(self) -> &'static str {
        match self {
            PadType::Xbox360 => include_str!(
                "../../../third-party/hidmaestro/profiles/microsoft/xbox-360-wired.json"
            ),
            PadType::XboxSeriesXS => include_str!(
                "../../../third-party/hidmaestro/profiles/microsoft/xbox-series-xs.json"
            ),
            PadType::DualShock4 => {
                include_str!("../../../third-party/hidmaestro/profiles/sony/dualshock-4-v2.json")
            }
            PadType::DualSense => {
                include_str!("../../../third-party/hidmaestro/profiles/sony/dualsense.json")
            }
            PadType::SwitchPro => {
                include_str!("../../../third-party/hidmaestro/profiles/nintendo/switch-pro.json")
            }
        }
    }

    /// Parse this family's vendored profile.
    pub fn profile(self) -> Option<Profile> {
        Profile::from_json(self.profile_json()).ok()
    }

    /// Build a session for this family.
    pub fn session(self) -> Result<PadSession, SessionError> {
        let profile = Profile::from_json(self.profile_json())
            .map_err(|e| SessionError::BadExtendedReport(e.to_string()))?;
        PadSession::new(&profile)
    }
}

/// Build a session for a wire `pad_type`, or `None` for an unknown code (the
/// server plugs nothing rather than guessing).
pub fn session_for(pad_type: u8) -> Option<PadSession> {
    PadType::from_u8(pad_type)?.session().ok()
}

/// The vendored profile for a wire `pad_type` — the identity (VID/PID, name) the
/// virtual device node is created from. `None` for an unknown code.
pub fn profile_for(pad_type: u8) -> Option<Profile> {
    PadType::from_u8(pad_type)?.profile()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunburst_core::proto::GamepadState;

    #[test]
    fn every_pad_type_builds_a_session_that_encodes() {
        for code in 0..=4u8 {
            let ty = PadType::from_u8(code).expect("known code");
            let mut session = ty.session().unwrap_or_else(|e| panic!("{ty:?}: {e}"));
            let report = session.submit(&GamepadState::default());
            assert!(!report.is_empty(), "{ty:?} produced an empty report");
        }
    }

    #[test]
    fn an_unknown_pad_type_yields_no_session() {
        assert!(session_for(200).is_none());
        assert!(PadType::from_u8(200).is_none());
    }

    #[test]
    fn xbox_family_packs_a_gip_buffer_but_sony_does_not() {
        let g = GamepadState::default();
        assert!(PadType::XboxSeriesXS.session().unwrap().gip(&g).is_some());
        assert!(PadType::Xbox360.session().unwrap().gip(&g).is_some());
        assert!(PadType::DualSense.session().unwrap().gip(&g).is_none());
    }
}

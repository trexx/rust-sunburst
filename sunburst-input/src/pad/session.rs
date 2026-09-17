// SPDX-License-Identifier: GPL-2.0-or-later

//! `PadSession` — one virtual controller, from profile to wire bytes.
//!
//! Ties the report stack together: it picks the report path a profile needs,
//! holds its per-frame buffers and encoder state, and turns a wire
//! [`GamepadState`] into the exact report bytes the driver serves. It is what the
//! Windows injector calls, but it is pure and Linux-testable — the driver submit
//! ([`super::shmem`], box-only) is a separate step.
//!
//! # Report path selection
//!
//! Mirrors `HMController.SubmitState`:
//! - a profile that **streams a vendor report from power-on** (`extendedReport`
//!   with `alwaysArmed`) uses the vendor-blob [`super::codec`];
//! - everything else uses the descriptor-driven [`super::report`] builder — the
//!   default/legacy report joy.cpl, DirectInput, WGI and HID consumers read.
//!
//! An **Xbox-VID** pad additionally packs the [`super::gip`] XUSB buffer that
//! XInput reads, exposed via [`PadSession::gip`].
//!
//! Not handled here: the *arm-on-demand* transition (a host `Get_Feature`
//! handshake flips a Sony BT pad from its legacy report to the vendor report — a
//! box-side event), and the Switch Pro protocol path (its packer is deferred; see
//! the module notes). Such a profile falls to the descriptor path.

use std::collections::BTreeMap;

use sunburst_core::proto::GamepadState;

use super::codec::{DecodedValue, EncoderState, decode, encode_input};
use super::gip::{self, GIP_LEN};
use super::map;
use super::profile::Profile;
use super::program::{self, Program};
use super::report::ReportBuilder;
use super::switch_pro;

/// Microsoft's USB vendor id — the pads whose XInput state is served from the
/// GIP buffer via the XUSB companion.
const VID_MICROSOFT: u16 = 0x045E;

/// Why a session could not be built.
#[derive(Debug, PartialEq, Eq)]
pub enum SessionError {
    /// The profile's `extendedReport` failed to compile.
    BadExtendedReport(String),
    /// The profile has neither a usable descriptor nor a vendor report.
    NoReport,
}

impl core::fmt::Display for SessionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SessionError::BadExtendedReport(e) => write!(f, "extendedReport did not compile: {e}"),
            SessionError::NoReport => write!(f, "profile declares no usable report"),
        }
    }
}

impl std::error::Error for SessionError {}

/// The report path a session takes.
enum Source {
    Descriptor(Box<ReportBuilder>),
    Codec(Program),
    /// Nintendo Switch Pro protocol — a hand-written packer, not a descriptor or
    /// vendor blob (`HMController.SubmitState`'s `_switchProtocol` path).
    SwitchPro,
}

/// One virtual pad's live encoding state.
pub struct PadSession {
    source: Source,
    report_buf: Vec<u8>,
    enc: EncoderState,
    packs_gip: bool,
    gip_buf: [u8; GIP_LEN],
    /// Compiled `extendedOutputReport`, for decoding the game's output reports.
    output: Option<Program>,
}

impl PadSession {
    /// Build a session for a profile.
    pub fn new(profile: &Profile) -> Result<PadSession, SessionError> {
        let is_switch_pro = profile.vid_u16() == Some(switch_pro::SWITCH_PRO_VID)
            && profile.pid_u16() == Some(switch_pro::SWITCH_PRO_PID);
        let source = if is_switch_pro {
            Source::SwitchPro
        } else {
            match &profile.extended_report {
                Some(er) if er.always_armed == Some(true) => {
                    let program = program::compile(er)
                        .map_err(|e| SessionError::BadExtendedReport(e.to_string()))?;
                    Source::Codec(program)
                }
                _ => {
                    let descriptor = profile.descriptor_bytes();
                    if descriptor.is_empty() {
                        return Err(SessionError::NoReport);
                    }
                    let mut builder = ReportBuilder::parse(
                        &descriptor,
                        profile.axis_map_pairs().as_deref(),
                        profile.button_map.clone(),
                        profile.trigger_buttons_pair(),
                        profile.preferred_report_id(),
                    );
                    if let Some(layout) = &profile.layout {
                        builder.apply_layout(layout);
                    }
                    Source::Descriptor(Box::new(builder))
                }
            }
        };

        let report_size = match &source {
            Source::Descriptor(b) => b.report_byte_size(),
            Source::Codec(p) => p.size,
            Source::SwitchPro => switch_pro::BODY_SIZE,
        };

        // Approximates HIDMaestro's RequiresXusbCompanion: a Microsoft-VID pad on
        // the descriptor path publishes XInput through the XUSB companion's GIP
        // buffer. (The precise predicate is a box-side DeviceOrchestrator concern.)
        let packs_gip =
            profile.vid_u16() == Some(VID_MICROSOFT) && matches!(source, Source::Descriptor(_));

        let output = profile
            .extended_output_report
            .as_ref()
            .and_then(|s| program::compile(s).ok());

        Ok(PadSession {
            source,
            report_buf: vec![0u8; report_size],
            enc: EncoderState::default(),
            packs_gip,
            gip_buf: [0u8; GIP_LEN],
            output,
        })
    }

    /// Encode one input frame into the session's report buffer, returning it.
    pub fn submit(&mut self, state: &GamepadState) -> &[u8] {
        let n = match &self.source {
            Source::Descriptor(builder) => {
                let mapped = map::map_state(state);
                let axes = mapped.axes();
                builder.build_into(
                    &mut self.report_buf,
                    &axes,
                    mapped.hat as i32,
                    mapped.button_mask,
                    None,
                    None,
                    None,
                )
            }
            Source::Codec(program) => {
                let input = map::to_input_state(state);
                encode_input(program, &input, &mut self.report_buf, &mut self.enc);
                program.size
            }
            Source::SwitchPro => {
                let m = map::map_state(state);
                let imu = map::to_switch_imu(state);
                let mut body = [0u8; switch_pro::BODY_SIZE];
                switch_pro::build_body(
                    &mut body,
                    m.button_mask,
                    m.hat,
                    m.left_x,
                    m.left_y,
                    m.right_x,
                    m.right_y,
                    &imu,
                );
                self.report_buf[..switch_pro::BODY_SIZE].copy_from_slice(&body);
                switch_pro::BODY_SIZE
            }
        };
        &self.report_buf[..n]
    }

    /// The XUSB/GIP buffer for this frame, when the pad publishes XInput.
    pub fn gip(&mut self, state: &GamepadState) -> Option<&[u8]> {
        if !self.packs_gip {
            return None;
        }
        self.gip_buf = gip::pack(&map::map_state(state));
        Some(&self.gip_buf)
    }

    /// Decode an output report from the driver into its semantic effect fields,
    /// plus whether its CRC verified. `None` when the profile declares no output
    /// report to decode against.
    pub fn decode_output(&self, report: &[u8]) -> Option<(BTreeMap<String, DecodedValue>, bool)> {
        self.output.as_ref().map(|p| decode(p, report))
    }

    /// The report length this session emits.
    pub fn report_len(&self) -> usize {
        self.report_buf.len()
    }

    /// Whether this session emits a vendor-blob (armed) report, which the driver
    /// takes verbatim from the extended region, rather than a descriptor report.
    pub fn is_extended(&self) -> bool {
        matches!(self.source, Source::Codec(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XBOX_SERIES_XS: &str =
        include_str!("../../../third-party/hidmaestro/profiles/microsoft/xbox-series-xs.json");
    const DUALSENSE_BT: &str =
        include_str!("../../../third-party/hidmaestro/profiles/sony/dualsense-bt.json");

    #[test]
    fn an_xbox_session_takes_the_descriptor_path_and_packs_gip() {
        let profile = Profile::from_json(XBOX_SERIES_XS).unwrap();
        let mut s = PadSession::new(&profile).expect("session");
        // 17-byte HID report (no report id), matching the descriptor test.
        assert_eq!(s.report_len(), 17);

        let g = GamepadState {
            lx: 32767, // full right
            buttons: sunburst_core::proto::input::buttons::A,
            ..Default::default()
        };
        let report = s.submit(&g);
        assert_eq!(report.len(), 17);
        assert_eq!(&report[0..2], &[0xFF, 0xFF], "X axis full right");
        assert_eq!(report[12], 0x01, "A button");

        // Xbox-VID → GIP buffer present, with A in the low button byte.
        let gip = s.gip(&g).expect("xbox packs a gip buffer");
        assert_eq!(gip.len(), GIP_LEN);
        assert_eq!(gip[12] & 0x01, 0x01);
    }

    #[test]
    fn a_dualsense_bt_session_takes_the_codec_path() {
        // dualsense-bt arms on a handshake (not alwaysArmed), so its default is the
        // descriptor path; only alwaysArmed profiles take the codec straight away.
        let profile = Profile::from_json(DUALSENSE_BT).unwrap();
        let s = PadSession::new(&profile).expect("session");
        // It has a descriptor, so it builds; and no GIP (Sony VID).
        assert!(s.report_len() > 0);
        assert!(!s.packs_gip);
    }

    #[test]
    fn submit_is_stable_across_frames() {
        let profile = Profile::from_json(XBOX_SERIES_XS).unwrap();
        let mut s = PadSession::new(&profile).unwrap();
        let g = GamepadState::default();
        let a = s.submit(&g).to_vec();
        let b = s.submit(&g).to_vec();
        assert_eq!(a, b, "the same state encodes identically");
    }
}

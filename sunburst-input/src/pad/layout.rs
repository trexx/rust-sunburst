// SPDX-License-Identifier: GPL-2.0-or-later

//! A profile's authored `layout` — role-tagged axes for non-gamepad devices.
//!
//! Ported from HIDMaestro's `HMLayout` + `ApplyLayoutSemantics`. A gamepad's
//! layout is author-aligned with the classifier and needs nothing here; a wheel,
//! HOTAS, flight stick, or pedal set declares which descriptor axis is the wheel,
//! the throttle, each pedal, so its controls land in the classic stick/trigger
//! slots. [`ReportBuilder::apply_layout`](super::report::ReportBuilder::apply_layout)
//! consumes this after classification.
//!
//! Scope: the **axis role assignment** that affects report building and the
//! canonical-axes view. The layout's rich UI metadata (button-role catalog,
//! sub-module clustering) and the strict layout↔descriptor validator are
//! consumer/sanity concerns with no effect on the emitted bytes, so they are not
//! ported. Verified by unit test against a real wheel layout.

use serde::Deserialize;

/// A device's layout kind (`HMLayoutKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutKind {
    Unspecified,
    Gamepad,
    Joystick,
    FlightStick,
    Hotas,
    Wheel,
    Pedals,
    Shifter,
    Handbrake,
    SingleAxisAccessory,
    ArcadeStick,
    DancePad,
    Guitar,
    MotionWand,
    Remote,
    ControllerAdapter,
}

impl LayoutKind {
    fn from_str(s: &str) -> LayoutKind {
        match s {
            "gamepad" => LayoutKind::Gamepad,
            "joystick" => LayoutKind::Joystick,
            "flight_stick" => LayoutKind::FlightStick,
            "hotas" => LayoutKind::Hotas,
            "wheel" => LayoutKind::Wheel,
            "pedals" => LayoutKind::Pedals,
            "shifter" => LayoutKind::Shifter,
            "handbrake" => LayoutKind::Handbrake,
            "single_axis_accessory" => LayoutKind::SingleAxisAccessory,
            "arcade_stick" => LayoutKind::ArcadeStick,
            "dance_pad" => LayoutKind::DancePad,
            "guitar" => LayoutKind::Guitar,
            "motion_wand" => LayoutKind::MotionWand,
            "remote" => LayoutKind::Remote,
            "controller_adapter" => LayoutKind::ControllerAdapter,
            _ => LayoutKind::Unspecified,
        }
    }
}

/// An `{ "axis": "X", … }` reference (extra fields ignored).
#[derive(Debug, Clone, Deserialize)]
pub struct AxisRef {
    pub axis: String,
}

/// A stick's `{ "xAxis": "X", "yAxis": "Y" }`.
#[derive(Debug, Clone, Deserialize)]
pub struct StickSpec {
    #[serde(rename = "xAxis")]
    pub x: String,
    #[serde(rename = "yAxis", default)]
    pub y: Option<String>,
}

/// A pedal's `{ "axis": "Z", "role": "accelerator" }`.
#[derive(Debug, Clone, Deserialize)]
pub struct PedalSpec {
    pub axis: String,
    #[serde(default)]
    pub role: String,
}

/// The parsed `layout` block — the union of axis-bearing fields the role
/// assignment reads, across every kind (lenient: unused fields stay `None`).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Layout {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub stick: Option<StickSpec>,
    #[serde(default)]
    pub throttle: Option<AxisRef>,
    #[serde(default)]
    pub rudder: Option<AxisRef>,
    #[serde(rename = "throttlePrimary", default)]
    pub throttle_primary: Option<AxisRef>,
    #[serde(rename = "stickRudder", default)]
    pub stick_rudder: Option<AxisRef>,
    #[serde(default)]
    pub wheel: Option<AxisRef>,
    #[serde(default)]
    pub pedals: Vec<PedalSpec>,
    /// Top-level axis for handbrake / single-axis-accessory.
    #[serde(default)]
    pub axis: Option<String>,
}

impl Layout {
    /// The discriminated kind.
    pub fn kind(&self) -> LayoutKind {
        LayoutKind::from_str(&self.kind)
    }

    /// The axis a pedal with one of `roles` sits on (first match), as a handle.
    pub fn pedal_axis(&self, roles: &[&str]) -> Option<u16> {
        for p in &self.pedals {
            if roles.iter().any(|r| p.role.eq_ignore_ascii_case(r)) {
                return axis_handle(&p.axis);
            }
        }
        None
    }
}

/// Map an `HMAxis` name to its `(usage_page << 8) | usage` handle.
pub fn axis_handle(name: &str) -> Option<u16> {
    Some(match name {
        // Generic Desktop (page 0x01)
        "X" => 0x0130,
        "Y" => 0x0131,
        "Z" => 0x0132,
        "Rx" => 0x0133,
        "Ry" => 0x0134,
        "Rz" => 0x0135,
        "Slider" => 0x0136,
        "Dial" => 0x0137,
        "Wheel" => 0x0138,
        "Vx" => 0x0140,
        "Vy" => 0x0141,
        "Vz" => 0x0142,
        // Simulation Controls (page 0x02)
        "Aileron" => 0x02B0,
        "Elevator" => 0x02B8,
        "Rudder" => 0x02BA,
        "Throttle" => 0x02BB,
        "Accelerator" => 0x02C4,
        "Brake" => 0x02C5,
        "Clutch" => 0x02C6,
        "Steering" => 0x02C8,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wheel_layout_parses_its_axes_and_pedal_roles() {
        // The Logitech Driving Force GT layout, verbatim shape.
        let json = r#"{
            "kind":"wheel",
            "wheel":{"axis":"X","rotationDegrees":900,"forceFeedback":true},
            "pedals":[{"axis":"Z","role":"accelerator","type":"potentiometer"}]
        }"#;
        let l: Layout = serde_json::from_str(json).expect("parse");
        assert_eq!(l.kind(), LayoutKind::Wheel);
        assert_eq!(
            l.wheel.as_ref().map(|w| axis_handle(&w.axis)),
            Some(Some(0x0130))
        );
        assert_eq!(l.pedal_axis(&["accelerator", "throttle"]), Some(0x0132));
        assert_eq!(l.pedal_axis(&["brake"]), None);
    }

    #[test]
    fn a_gamepad_layout_is_recognized_as_author_aligned() {
        let l: Layout = serde_json::from_str(r#"{"kind":"gamepad","sticks":[]}"#).expect("parse");
        assert_eq!(l.kind(), LayoutKind::Gamepad);
    }

    #[test]
    fn axis_names_map_to_handles() {
        assert_eq!(axis_handle("X"), Some(0x0130));
        assert_eq!(axis_handle("Rz"), Some(0x0135));
        assert_eq!(axis_handle("Throttle"), Some(0x02BB));
        assert_eq!(axis_handle("nonsense"), None);
    }
}

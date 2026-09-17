// SPDX-License-Identifier: GPL-2.0-or-later

//! The shared front-end: wire [`GamepadState`] → device state every report path
//! consumes.
//!
//! One mapping feeds all four builders — the descriptor builder ([`super::report`]
//! via [`Mapped::axes`]), the XInput/GIP packer ([`super::gip`]), the vendor-blob
//! codec ([`super::codec`] via [`to_input_state`]), and the Switch Pro packer. It
//! resolves the wire's XInput-convention sticks/triggers/buttons into HIDMaestro's
//! canonical form: sticks and triggers as `[0, 1]`, buttons as an `HMButton` mask,
//! the d-pad as a hat octant.
//!
//! # Stick Y is inverted
//!
//! The wire carries XInput orientation (up = positive). HID gamepad reports put
//! stick up at the *minimum* — confirmed by HIDMaestro reading `LeftThumbstickY`
//! back with `invert: true` in its WGI factory — so Y is inverted here to match.
//! X, and both triggers, map straight through. This is the one axis decision whose
//! *feel* is verified on hardware; the orientation itself is pinned above.

use sunburst_core::proto::GamepadState;
use sunburst_core::proto::input::buttons as wire;

use super::codec::InputState;
use super::program::{Hat, button as hm};
use super::report::Axes;
use super::switch_pro::SwitchImu;

/// Wire IMU fixed-point scales — the canonical physical units the wire carries:
/// gyro in `dps × 16`, accel in `g × 4096` (which happen to match a DualSense's
/// native sensitivities, so the Sony codec path stays a near-passthrough).
pub const WIRE_GYRO_LSB_PER_DPS: f32 = 16.0;
pub const WIRE_ACCEL_LSB_PER_G: f32 = 4096.0;

/// Canonical HMAxis handles (`(usage_page << 8) | usage`) for the axes dict.
mod axis {
    pub const X: u16 = 0x0130;
    pub const Y: u16 = 0x0131;
    pub const RX: u16 = 0x0133;
    pub const RY: u16 = 0x0134;
    pub const Z: u16 = 0x0132;
    pub const RZ: u16 = 0x0135;
}

/// The resolved analog + digital state, in HIDMaestro's canonical form.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mapped {
    pub left_x: f32,
    pub left_y: f32,
    pub right_x: f32,
    pub right_y: f32,
    pub left_trigger: f32,
    pub right_trigger: f32,
    /// `HMButton` mask.
    pub button_mask: u32,
    /// Hat octant: 0 = neutral, 1..8 = N..NW.
    pub hat: u8,
}

/// `i16` stick axis → `[0, 1]`. XInput is `[-32768, 32767]`; `invert` flips Y so
/// stick-up lands at the HID minimum.
fn stick_norm(v: i16, invert: bool) -> f32 {
    let n = (v as i32 + 32768) as f32 / 65535.0;
    if invert { 1.0 - n } else { n }
}

/// `u8` trigger → `[0, 1]`.
fn trigger_norm(v: u8) -> f32 {
    v as f32 / 255.0
}

/// Wire (XInput-layout + extended) buttons → an `HMButton` mask. The d-pad is not
/// a button here — it becomes the hat (see [`dpad_to_hat`]).
pub fn buttons_to_hmbutton(w: u32) -> u32 {
    let mut m = 0u32;
    let mut set = |wire_bit: u32, hm_bit: u64| {
        if w & wire_bit != 0 {
            m |= hm_bit as u32;
        }
    };
    set(wire::A, hm::A);
    set(wire::B, hm::B);
    set(wire::X, hm::X);
    set(wire::Y, hm::Y);
    set(wire::LEFT_SHOULDER, hm::LEFT_BUMPER);
    set(wire::RIGHT_SHOULDER, hm::RIGHT_BUMPER);
    set(wire::BACK, hm::BACK);
    set(wire::START, hm::START);
    set(wire::LEFT_THUMB, hm::LEFT_STICK);
    set(wire::RIGHT_THUMB, hm::RIGHT_STICK);
    set(wire::GUIDE, hm::GUIDE);
    set(wire::TOUCHPAD_CLICK, hm::TOUCHPAD);
    set(wire::SHARE, hm::SHARE);
    set(wire::RIGHT_PADDLE, hm::RIGHT_PADDLE);
    set(wire::LEFT_PADDLE, hm::LEFT_PADDLE);
    set(wire::MISC1, hm::MISC1);
    set(wire::RIGHT_PADDLE2, hm::RIGHT_PADDLE2);
    set(wire::LEFT_PADDLE2, hm::LEFT_PADDLE2);
    m
}

/// Wire d-pad bits → hat octant. Diagonals combine; opposite pairs cancel to the
/// dominant single direction's absence (up+down with nothing else → neutral).
pub fn dpad_to_hat(w: u32) -> Hat {
    let up = w & wire::DPAD_UP != 0;
    let down = w & wire::DPAD_DOWN != 0;
    let left = w & wire::DPAD_LEFT != 0;
    let right = w & wire::DPAD_RIGHT != 0;
    // Cancel opposing pairs so up+down / left+right read as no vertical / no
    // horizontal, matching how a real hat resolves a conflicting press.
    let (up, down) = if up && down {
        (false, false)
    } else {
        (up, down)
    };
    let (left, right) = if left && right {
        (false, false)
    } else {
        (left, right)
    };
    match (up, right, down, left) {
        (true, false, false, false) => Hat::North,
        (true, true, false, false) => Hat::NorthEast,
        (false, true, false, false) => Hat::East,
        (false, true, true, false) => Hat::SouthEast,
        (false, false, true, false) => Hat::South,
        (false, false, true, true) => Hat::SouthWest,
        (false, false, false, true) => Hat::West,
        (true, false, false, true) => Hat::NorthWest,
        _ => Hat::None,
    }
}

/// Map a wire [`GamepadState`] to the canonical analog + digital form.
pub fn map_state(g: &GamepadState) -> Mapped {
    Mapped {
        left_x: stick_norm(g.lx, false),
        left_y: stick_norm(g.ly, true),
        right_x: stick_norm(g.rx, false),
        right_y: stick_norm(g.ry, true),
        left_trigger: trigger_norm(g.lt),
        right_trigger: trigger_norm(g.rt),
        button_mask: buttons_to_hmbutton(g.buttons),
        hat: dpad_to_hat(g.buttons) as u8,
    }
}

impl Mapped {
    /// The canonical axes dict for the descriptor builder: left stick on X/Y, right
    /// stick on Rx/Ry, triggers on Z/Rz (the Xbox convention; a profile's `axisMap`
    /// re-routes per family, e.g. Sony's Z/Rz right stick).
    pub fn axes(&self) -> Axes {
        let mut a = Axes::with_capacity(6);
        a.insert(axis::X, self.left_x);
        a.insert(axis::Y, self.left_y);
        a.insert(axis::RX, self.right_x);
        a.insert(axis::RY, self.right_y);
        a.insert(axis::Z, self.left_trigger);
        a.insert(axis::RZ, self.right_trigger);
        a
    }
}

/// Convert the wire's canonical IMU (gyro `dps × 16`, accel `g × 4096`) to the
/// physical dps/g the Switch Pro packer takes. Absent IMU → zero. The wire→SDL
/// axis frame here (gyro pitch/yaw/roll → X/Y/Z, accel x/y/z → X/Y/Z) is
/// box-verified against a real controller.
pub fn to_switch_imu(g: &GamepadState) -> SwitchImu {
    let Some(imu) = g.imu else {
        return SwitchImu::default();
    };
    SwitchImu {
        gyro_x: imu.gyro_pitch as f32 / WIRE_GYRO_LSB_PER_DPS,
        gyro_y: imu.gyro_yaw as f32 / WIRE_GYRO_LSB_PER_DPS,
        gyro_z: imu.gyro_roll as f32 / WIRE_GYRO_LSB_PER_DPS,
        accel_x: imu.accel_x as f32 / WIRE_ACCEL_LSB_PER_G,
        accel_y: imu.accel_y as f32 / WIRE_ACCEL_LSB_PER_G,
        accel_z: imu.accel_z as f32 / WIRE_ACCEL_LSB_PER_G,
    }
}

/// Map a wire [`GamepadState`] to the vendor-blob codec's [`InputState`], carrying
/// the rich sections (IMU, touchpad, battery) through. The codec's IMU fields are
/// raw device units; the wire's canonical scale matches the DualSense's, so this
/// stays a passthrough (the fine calibration is box-tuned).
pub fn to_input_state(g: &GamepadState) -> InputState {
    let m = map_state(g);
    let mut s = InputState {
        left_stick_x: m.left_x,
        left_stick_y: m.left_y,
        right_stick_x: m.right_x,
        right_stick_y: m.right_y,
        left_trigger: m.left_trigger,
        right_trigger: m.right_trigger,
        buttons: m.button_mask,
        hat: dpad_to_hat(g.buttons),
        ..Default::default()
    };
    if let Some(imu) = g.imu {
        s.gyro_pitch = imu.gyro_pitch;
        s.gyro_yaw = imu.gyro_yaw;
        s.gyro_roll = imu.gyro_roll;
        s.accel_x = imu.accel_x;
        s.accel_y = imu.accel_y;
        s.accel_z = imu.accel_z;
        s.sensor_timestamp = imu.sensor_timestamp;
    }
    if let Some(tp) = g.touchpad {
        s.finger0_active = tp.finger0.active;
        s.finger0_x = tp.finger0.x;
        s.finger0_y = tp.finger0.y;
        s.finger0_id = tp.finger0.id;
        s.finger1_active = tp.finger1.active;
        s.finger1_x = tp.finger1.x;
        s.finger1_y = tp.finger1.y;
        s.finger1_id = tp.finger1.id;
    }
    if let Some(bat) = g.battery {
        s.battery_level = bat.level;
        s.battery_charging = bat.charging;
        s.battery_full = bat.full;
        s.mic_muted = bat.mic_muted;
        s.headphones_connected = bat.headphones;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunburst_core::proto::{Battery, Finger, Imu, Touchpad};

    #[test]
    fn sticks_normalize_and_y_inverts() {
        // X: full-left → 0, full-right → 1, centre → ~0.5.
        assert_eq!(stick_norm(-32768, false), 0.0);
        assert_eq!(stick_norm(32767, false), 1.0);
        assert!((stick_norm(0, false) - 0.5).abs() < 0.001);
        // Y inverted: XInput up (+32767) → HID min (0); down (-32768) → 1.
        assert_eq!(stick_norm(32767, true), 0.0);
        assert_eq!(stick_norm(-32768, true), 1.0);
    }

    #[test]
    fn triggers_normalize() {
        assert_eq!(trigger_norm(0), 0.0);
        assert_eq!(trigger_norm(255), 1.0);
    }

    #[test]
    fn xinput_buttons_remap_to_hmbutton() {
        // Wire A + Y + Guide + Share.
        let w = wire::A | wire::Y | wire::GUIDE | wire::SHARE;
        let m = buttons_to_hmbutton(w);
        assert_eq!(m & hm::A as u32, hm::A as u32);
        assert_eq!(m & hm::Y as u32, hm::Y as u32);
        assert_eq!(m & hm::GUIDE as u32, hm::GUIDE as u32);
        assert_eq!(m & hm::SHARE as u32, hm::SHARE as u32);
        // B/X not set.
        assert_eq!(m & hm::B as u32, 0);
        assert_eq!(m & hm::X as u32, 0);
        // The d-pad is not a button.
        assert_eq!(buttons_to_hmbutton(wire::DPAD_UP), 0);
    }

    #[test]
    fn dpad_resolves_to_octants() {
        assert_eq!(dpad_to_hat(wire::DPAD_UP), Hat::North);
        assert_eq!(
            dpad_to_hat(wire::DPAD_UP | wire::DPAD_RIGHT),
            Hat::NorthEast
        );
        assert_eq!(dpad_to_hat(wire::DPAD_RIGHT), Hat::East);
        assert_eq!(
            dpad_to_hat(wire::DPAD_DOWN | wire::DPAD_LEFT),
            Hat::SouthWest
        );
        assert_eq!(dpad_to_hat(wire::DPAD_UP | wire::DPAD_LEFT), Hat::NorthWest);
        assert_eq!(dpad_to_hat(0), Hat::None);
        // Opposing pairs cancel.
        assert_eq!(dpad_to_hat(wire::DPAD_UP | wire::DPAD_DOWN), Hat::None);
    }

    #[test]
    fn axes_use_the_canonical_convention() {
        let g = GamepadState {
            lx: 32767, // full right
            ly: 32767, // up (inverts to 0)
            rx: -32768,
            ry: -32768, // down (inverts to 1)
            lt: 255,
            rt: 0,
            ..Default::default()
        };
        let a = map_state(&g).axes();
        assert_eq!(a[&axis::X], 1.0);
        assert_eq!(a[&axis::Y], 0.0);
        assert_eq!(a[&axis::RX], 0.0);
        assert_eq!(a[&axis::RY], 1.0);
        assert_eq!(a[&axis::Z], 1.0);
        assert_eq!(a[&axis::RZ], 0.0);
    }

    #[test]
    fn rich_sections_carry_into_input_state() {
        let g = GamepadState {
            buttons: wire::A,
            imu: Some(Imu {
                gyro_pitch: 100,
                gyro_yaw: -200,
                gyro_roll: 300,
                accel_x: 1,
                accel_y: 2,
                accel_z: 3,
                sensor_timestamp: 0xDEAD,
            }),
            touchpad: Some(Touchpad {
                finger0: Finger {
                    active: true,
                    x: 960,
                    y: 540,
                    id: 3,
                },
                finger1: Finger::default(),
            }),
            battery: Some(Battery {
                level: 8,
                charging: true,
                full: false,
                mic_muted: true,
                headphones: true,
            }),
            ..Default::default()
        };
        let s = to_input_state(&g);
        assert_eq!(s.buttons, hm::A as u32);
        assert_eq!(s.gyro_pitch, 100);
        assert_eq!(s.sensor_timestamp, 0xDEAD);
        assert!(s.finger0_active);
        assert_eq!(s.finger0_x, 960);
        assert_eq!(s.battery_level, 8);
        assert!(s.battery_charging);
        assert!(s.mic_muted);
    }

    #[test]
    fn switch_imu_converts_canonical_to_physical() {
        // gyro 1600 LSB / 16 = 100 dps; accel 4096 LSB / 4096 = 1.0 g.
        let g = GamepadState {
            imu: Some(Imu {
                gyro_pitch: 1600,
                gyro_yaw: -320,
                gyro_roll: 16,
                accel_x: 4096,
                accel_y: -2048,
                accel_z: 0,
                sensor_timestamp: 0,
            }),
            ..Default::default()
        };
        let s = to_switch_imu(&g);
        assert_eq!(s.gyro_x, 100.0);
        assert_eq!(s.gyro_y, -20.0);
        assert_eq!(s.gyro_z, 1.0);
        assert_eq!(s.accel_x, 1.0);
        assert_eq!(s.accel_y, -0.5);
        assert_eq!(s.accel_z, 0.0);
        // No IMU → zeroed.
        assert_eq!(
            to_switch_imu(&GamepadState::default()),
            SwitchImu::default()
        );
    }
}

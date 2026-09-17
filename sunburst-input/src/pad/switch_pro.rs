// SPDX-License-Identifier: GPL-2.0-or-later

//! The Nintendo Switch Pro wire packer — the fourth report path.
//!
//! Ported from HIDMaestro's `Internal/SwitchProPacker`. The Switch Pro's report
//! `0x30` body is a hand-written protocol, not a descriptor or vendor-blob layout,
//! so it gets its own packer: buttons in Nintendo's bit order, two 12-bit sticks,
//! and three IMU frames whose SDL sensor frame is converted to the Switch wire
//! frame here. The driver's 60 Hz streamer serves the body with its own
//! counter/battery overlay.
//!
//! IMU input is **physical units** — gyro in dps, accel in g — which is what the
//! wire now carries ([`super::map::to_switch_imu`]). The axis frame and the exact
//! gyro scale are box-verified against a real controller (the "feel" item); the
//! layout is byte-exact and host-tested.

/// The report-0x30 body length.
pub const BODY_SIZE: usize = 48;

/// Switch Pro identity.
pub const SWITCH_PRO_VID: u16 = 0x057E;
pub const SWITCH_PRO_PID: u16 = 0x2009;

/// One IMU sample in physical units (SDL sensor frame): gyro dps, accel g.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SwitchImu {
    pub gyro_x: f32,
    pub gyro_y: f32,
    pub gyro_z: f32,
    pub accel_x: f32,
    pub accel_y: f32,
    pub accel_z: f32,
}

fn bit(mask: u32, index: u32) -> u32 {
    (mask >> index) & 1
}

// Wire bits per SDL HandleFullControllerState (HMButton mask → Switch wire order).
fn buttons_byte0(m: u32) -> u8 {
    (bit(m, 2)
        | (bit(m, 3) << 1)
        | (bit(m, 0) << 2)
        | (bit(m, 1) << 3)
        | (bit(m, 5) << 6)
        | (bit(m, 7) << 7)) as u8
}
fn buttons_byte1(m: u32) -> u8 {
    (bit(m, 8)
        | (bit(m, 9) << 1)
        | (bit(m, 11) << 2)
        | (bit(m, 10) << 3)
        | (bit(m, 12) << 4)
        | (bit(m, 13) << 5)) as u8
}
fn buttons_byte2(m: u32, up: bool, down: bool, left: bool, right: bool) -> u8 {
    (u32::from(down)
        | (u32::from(up) << 1)
        | (u32::from(right) << 2)
        | (u32::from(left) << 3)
        | (bit(m, 4) << 6)
        | (bit(m, 6) << 7)) as u8
}

/// Resolve a hat octant (0 = neutral, 1..8 = N..NW) into d-pad booleans, using the
/// same degree windows the packer does.
fn resolve_dpad(hat: u8) -> (bool, bool, bool, bool) {
    if !(1..=8).contains(&hat) {
        return (false, false, false, false);
    }
    let d = (hat as f64 - 1.0) * 45.0;
    let up = !(67.5..=292.5).contains(&d); // wraps through 0°/360°
    let right = d > 22.5 && d < 157.5;
    let down = d > 112.5 && d < 247.5;
    let left = d > 202.5 && d < 337.5;
    (up, down, left, right)
}

/// 12-bit stick value: `[0,1]` centred at 0.5, centre 0x800, range 0x600.
fn stick_raw(v: f32, invert: bool) -> u16 {
    let mut centered = (v.clamp(0.0, 1.0) * 2.0) - 1.0;
    if invert {
        centered = -centered;
    }
    let raw = 0x800 + (centered * 0x600 as f32) as i32;
    raw.clamp(0, 0xFFF) as u16
}

fn pack_stick(dst: &mut [u8], offset: usize, x: u16, y: u16) {
    dst[offset] = (x & 0xFF) as u8;
    dst[offset + 1] = (((x >> 8) & 0x0F) | ((y & 0x0F) << 4)) as u8;
    dst[offset + 2] = (y >> 4) as u8;
}

fn imu_raw(value: f32, scale: f32) -> i16 {
    ((value * scale).round() as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

fn write_i16(dst: &mut [u8], offset: usize, v: i16) {
    dst[offset] = (v & 0xFF) as u8;
    dst[offset + 1] = ((v >> 8) & 0xFF) as u8;
}

/// Gyro raw = dps × 13371/936, the inverse of SDL's LoadIMUCalibration with the
/// coefficients the driver's fabricated SPI image serves.
const GYRO_SCALE: f32 = 13371.0 / 936.0;
/// Accel raw = g × 4096, matching the fabricated factory calibration.
const ACCEL_SCALE: f32 = 4096.0;

/// Build the 48-byte report-0x30 body. `buttons` is the HMButton mask, `hat` the
/// octant; sticks are `[0,1]` (0.5 = centre); `imu` is one physical sample.
///
/// `dst[0]` (counter), `dst[1]` (battery) and `dst[11]` (vibrator) are overlaid by
/// the driver's streamer and left zero here.
#[allow(clippy::too_many_arguments)]
pub fn build_body(
    dst: &mut [u8; BODY_SIZE],
    buttons: u32,
    hat: u8,
    lx: f32,
    ly: f32,
    rx: f32,
    ry: f32,
    imu: &SwitchImu,
) {
    *dst = [0u8; BODY_SIZE];

    let (up, down, left, right) = resolve_dpad(hat);
    dst[2] = buttons_byte0(buttons);
    dst[3] = buttons_byte1(buttons);
    dst[4] = buttons_byte2(buttons, up, down, left, right);

    // Wire wants up-positive on Y, opposite the byte-oriented pads, so Y inverts.
    pack_stick(dst, 5, stick_raw(lx, false), stick_raw(ly, true));
    pack_stick(dst, 8, stick_raw(rx, false), stick_raw(ry, true));

    // SDL SendSensorUpdate maps wire→SDL as sdl0=-wireY, sdl1=+wireZ, sdl2=-wireX;
    // the inverse packed here is wireX=-sdlZ, wireY=-sdlX, wireZ=+sdlY.
    let ax = imu_raw(-imu.accel_z, ACCEL_SCALE);
    let ay = imu_raw(-imu.accel_x, ACCEL_SCALE);
    let az = imu_raw(imu.accel_y, ACCEL_SCALE);
    let gx = imu_raw(-imu.gyro_z, GYRO_SCALE);
    let gy = imu_raw(-imu.gyro_x, GYRO_SCALE);
    let gz = imu_raw(imu.gyro_y, GYRO_SCALE);
    for frame in 0..3 {
        let o = 12 + frame * 12;
        write_i16(dst, o, ax);
        write_i16(dst, o + 2, ay);
        write_i16(dst, o + 4, az);
        write_i16(dst, o + 6, gx);
        write_i16(dst, o + 8, gy);
        write_i16(dst, o + 10, gz);
    }
}

/// Coarse per-side rumble amplitude (0..255) from one 4-byte HD-rumble block,
/// taking the louder of the high- and low-band amplitudes. Neutral / zero → 0.
pub fn decode_rumble_amplitude(block: &[u8]) -> u8 {
    if block.len() < 4 {
        return 0;
    }
    let hf = (block[1] & 0xFE) as i32;
    let hf_norm = (hf * 255 / 0xC8).clamp(0, 255);

    let msb = i32::from(block[2] & 0x80 != 0);
    let lo = block[3] as i32;
    let mut lf_norm = 0;
    if (0x40..=0x72).contains(&lo) {
        let index = (lo - 0x40) * 2 + msb;
        lf_norm = (index * 255 / 101).clamp(0, 255);
    }
    hf_norm.max(lf_norm) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pad::program::button;

    #[test]
    fn a_centered_neutral_body_is_mostly_zero_with_stick_centers() {
        let mut dst = [0u8; BODY_SIZE];
        build_body(&mut dst, 0, 0, 0.5, 0.5, 0.5, 0.5, &SwitchImu::default());
        assert_eq!(dst[2..5], [0, 0, 0], "no buttons");
        // Centre stick = 0x800: [lo]=0x00, [mid]=0x08, [hi]=0x80.
        assert_eq!(dst[5..8], [0x00, 0x08, 0x80]);
        assert_eq!(dst[8..11], [0x00, 0x08, 0x80]);
    }

    #[test]
    fn buttons_pack_in_nintendo_wire_order() {
        // HMButton A (bit 0) → byte0 bit2; Start (bit 7) → byte0 bit7.
        let mut dst = [0u8; BODY_SIZE];
        build_body(
            &mut dst,
            button::A as u32 | button::START as u32,
            0,
            0.5,
            0.5,
            0.5,
            0.5,
            &SwitchImu::default(),
        );
        assert_eq!(dst[2], 0b1000_0100);
        // LeftBumper (bit 4) → byte2 bit6; Back (bit 6) → byte2 bit7.
        let mut d2 = [0u8; BODY_SIZE];
        build_body(
            &mut d2,
            button::LEFT_BUMPER as u32 | button::BACK as u32,
            0,
            0.5,
            0.5,
            0.5,
            0.5,
            &SwitchImu::default(),
        );
        assert_eq!(d2[4], 0b1100_0000);
    }

    #[test]
    fn the_dpad_octants_set_the_right_bits() {
        // North → up only (byte2 bit1); East → right only (bit2).
        let mut n = [0u8; BODY_SIZE];
        build_body(&mut n, 0, 1, 0.5, 0.5, 0.5, 0.5, &SwitchImu::default());
        assert_eq!(n[4] & 0x0F, 0b0010, "north = up");
        let mut e = [0u8; BODY_SIZE];
        build_body(&mut e, 0, 3, 0.5, 0.5, 0.5, 0.5, &SwitchImu::default());
        assert_eq!(e[4] & 0x0F, 0b0100, "east = right");
        // NorthEast → up + right.
        let mut ne = [0u8; BODY_SIZE];
        build_body(&mut ne, 0, 2, 0.5, 0.5, 0.5, 0.5, &SwitchImu::default());
        assert_eq!(ne[4] & 0x0F, 0b0110, "northeast = up + right");
    }

    #[test]
    fn imu_frames_repeat_three_times() {
        let mut dst = [0u8; BODY_SIZE];
        let imu = SwitchImu {
            gyro_x: 100.0,
            gyro_y: -50.0,
            gyro_z: 25.0,
            accel_x: 0.5,
            accel_y: -1.0,
            accel_z: 0.25,
        };
        build_body(&mut dst, 0, 0, 0.5, 0.5, 0.5, 0.5, &imu);
        // The three 12-byte frames are identical.
        assert_eq!(dst[12..24], dst[24..36]);
        assert_eq!(dst[24..36], dst[36..48]);
        // accel_z 0.25 g → ax = -accel_z*4096 = -1024 = 0xFC00 LE.
        assert_eq!(&dst[12..14], &[0x00, 0xFC]);
    }

    #[test]
    fn rumble_amplitude_decodes_bands() {
        // Neutral block (00 01 40 40) → 0.
        assert_eq!(decode_rumble_amplitude(&[0x00, 0x01, 0x40, 0x40]), 0);
        // High-band amplitude 0xC8 → full scale.
        assert_eq!(decode_rumble_amplitude(&[0x00, 0xC8, 0x40, 0x40]), 255);
        // Too short → 0.
        assert_eq!(decode_rumble_amplitude(&[0x00, 0x01]), 0);
    }
}

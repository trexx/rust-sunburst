// SPDX-License-Identifier: GPL-2.0-or-later

//! The XUSB/GIP state buffer — what XInput games read for an Xbox pad.
//!
//! Transcribed verbatim from HIDMaestro's `HMController.SubmitState` Xbox-VID
//! branch. An Xbox-family virtual pad publishes two things: the HID report (built
//! by [`super::report`], read by HID/WGI/DirectInput consumers) **and** this fixed
//! 14-byte buffer, which the XUSB companion serves on `IOCTL_XUSB_GET_STATE` so
//! `XInputGetState(Ex)` — the path Steam and classic XInput games use — sees the
//! pad. Sticks are full-scale unsigned 16-bit, triggers 10-bit, buttons the XInput
//! XUSB byte convention with the hat packed into the high button byte.
//!
//! Deterministic and self-contained, so it is pinned by a hand-computed test.

use super::map::Mapped;
use super::program::button;

/// The 14-byte GIP buffer length.
pub const GIP_LEN: usize = 14;

/// Pack the XUSB/GIP state buffer from the canonical [`Mapped`] state.
pub fn pack(m: &Mapped) -> [u8; GIP_LEN] {
    let mut buf = [0u8; GIP_LEN];

    let gip_lx = (m.left_x.clamp(0.0, 1.0) * 65535.0) as u16;
    let gip_ly = (m.left_y.clamp(0.0, 1.0) * 65535.0) as u16;
    let gip_rx = (m.right_x.clamp(0.0, 1.0) * 65535.0) as u16;
    let gip_ry = (m.right_y.clamp(0.0, 1.0) * 65535.0) as u16;
    let gip_lt = (m.left_trigger.clamp(0.0, 1.0) * 1023.0) as u16;
    let gip_rt = (m.right_trigger.clamp(0.0, 1.0) * 1023.0) as u16;

    buf[0..2].copy_from_slice(&gip_lx.to_le_bytes());
    buf[2..4].copy_from_slice(&gip_ly.to_le_bytes());
    buf[4..6].copy_from_slice(&gip_rx.to_le_bytes());
    buf[6..8].copy_from_slice(&gip_ry.to_le_bytes());
    buf[8..10].copy_from_slice(&gip_lt.to_le_bytes());
    buf[10..12].copy_from_slice(&gip_rt.to_le_bytes());

    // Button low byte: A,B,X,Y,LB,RB,LS,RS (XInput XUSB convention).
    let b = m.button_mask;
    let mut btn_low = 0u8;
    if b & button::A as u32 != 0 {
        btn_low |= 0x01;
    }
    if b & button::B as u32 != 0 {
        btn_low |= 0x02;
    }
    if b & button::X as u32 != 0 {
        btn_low |= 0x04;
    }
    if b & button::Y as u32 != 0 {
        btn_low |= 0x08;
    }
    if b & button::LEFT_BUMPER as u32 != 0 {
        btn_low |= 0x10;
    }
    if b & button::RIGHT_BUMPER as u32 != 0 {
        btn_low |= 0x20;
    }
    if b & button::LEFT_STICK as u32 != 0 {
        btn_low |= 0x40;
    }
    if b & button::RIGHT_STICK as u32 != 0 {
        btn_low |= 0x80;
    }
    buf[12] = btn_low;

    // Button high byte: Back(0x01), Start(0x02), the 4-bit hat at bits 2-5
    // (companion.c reads `(btnHigh >> 2) & 0x0F` into wButtons.DPAD_*), Guide(0x40).
    let mut btn_high = 0u8;
    if b & button::BACK as u32 != 0 {
        btn_high |= 0x01;
    }
    if b & button::START as u32 != 0 {
        btn_high |= 0x02;
    }
    btn_high |= (m.hat & 0x0F) << 2;
    if b & button::GUIDE as u32 != 0 {
        btn_high |= 0x40;
    }
    buf[13] = btn_high;

    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapped(button_mask: u32, hat: u8, triggers_full: bool, sticks_full: bool) -> Mapped {
        let s = if sticks_full { 1.0 } else { 0.0 };
        let t = if triggers_full { 1.0 } else { 0.0 };
        Mapped {
            left_x: s,
            left_y: s,
            right_x: s,
            right_y: s,
            left_trigger: t,
            right_trigger: t,
            button_mask,
            hat,
        }
    }

    #[test]
    fn neutral_is_all_zero() {
        assert_eq!(pack(&mapped(0, 0, false, false)), [0u8; GIP_LEN]);
    }

    #[test]
    fn full_scale_sticks_and_triggers() {
        let buf = pack(&mapped(0, 0, true, true));
        // Sticks 65535 = 0xFFFF LE.
        assert_eq!(
            &buf[0..8],
            &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]
        );
        // Triggers 1023 = 0x03FF LE.
        assert_eq!(&buf[8..12], &[0xFF, 0x03, 0xFF, 0x03]);
    }

    #[test]
    fn buttons_pack_to_the_xusb_convention() {
        let mask = button::A as u32
            | button::LEFT_BUMPER as u32
            | button::START as u32
            | button::GUIDE as u32;
        let buf = pack(&mapped(mask, 0, false, false));
        assert_eq!(buf[12], 0x01 | 0x10, "A + LB in low byte");
        assert_eq!(buf[13], 0x02 | 0x40, "Start + Guide in high byte");
    }

    #[test]
    fn hat_octant_sits_in_the_high_byte() {
        // NorthEast = octant 2 → bits 2-5 = 2 << 2 = 0x08.
        let buf = pack(&mapped(0, 2, false, false));
        assert_eq!(buf[13], 0x08);
        assert_eq!((buf[13] >> 2) & 0x0F, 2, "companion reads the octant back");
        // NorthWest = octant 8 → 8 << 2 = 0x20; must not smear into Guide (0x40).
        assert_eq!(pack(&mapped(0, 8, false, false))[13], 0x20);
    }
}

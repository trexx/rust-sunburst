// SPDX-License-Identifier: GPL-2.0-or-later

//! The descriptor path's proof: parse a *real* vendored HID descriptor and pack a
//! report, asserting the exact bytes.
//!
//! Unlike the vendor-blob codec (63 committed SHA hashes), the descriptor builder
//! has no golden table upstream, and there is no .NET here to generate one. So this
//! is the plan's second oracle: **hand-decode** the real Xbox Series X|S descriptor
//! and assert the parser/builder reproduce it. The descriptor is HIDMaestro's,
//! vendored verbatim under `third-party/hidmaestro/`.

use super::profile::Profile;
use super::report::ReportBuilder;

const XBOX_SERIES_XS: &str =
    include_str!("../../../third-party/hidmaestro/profiles/microsoft/xbox-series-xs.json");
const DRIVING_FORCE_GT: &str =
    include_str!("../../../third-party/hidmaestro/profiles/logitech/driving-force-gt.json");

/// Hand-decode of the Xbox Series X|S input report (report id 0 — none declared):
/// ```text
///   X   usage 0x30  bit 0    16 bits  [0..65535]
///   Y   usage 0x31  bit 16   16 bits
///   Rx  usage 0x33  bit 32   16 bits
///   Ry  usage 0x34  bit 48   16 bits
///   Z   usage 0x32  bit 64   10 bits  [0..1023]   (+6 bits const pad)
///   Rz  usage 0x35  bit 80   10 bits              (+6 bits const pad)
///   Buttons 1..12   bit 96   1 bit each           (+4 bits const pad)
///   Hat usage 0x39  bit 112  4 bits   [1..8]      (+4 bits const pad)
///   SystemMainMenu (Guide) usage 0x85  bit 120  1 bit  (+7 bits const pad)
///   Battery Strength (page 0x06/0x20)  bit 128  8 bits  (unclassified)
///   → 136 bits = 17 bytes, no report-id byte
/// ```
#[test]
fn xbox_series_xs_descriptor_packs_the_expected_report() {
    let profile = Profile::from_json(XBOX_SERIES_XS).expect("parse profile");
    let builder = ReportBuilder::parse(
        &profile.descriptor_bytes(),
        profile.axis_map_pairs().as_deref(),
        profile.button_map.clone(),
        profile.trigger_buttons_pair(),
        profile.preferred_report_id(),
    );

    // No report id declared → 17-byte report, byte 0 is the X axis.
    assert_eq!(builder.input_report_id, 0);
    assert_eq!(builder.report_byte_size(), 17);

    let mut report = vec![0u8; builder.report_byte_size()];
    // LX full-right (1.0→65535), LY centred (0.5→32767), RX/RY centred, LT full
    // (1.0→1023), RT released. Buttons: A (HMButton bit 0 → descriptor button 0).
    let axes = builder.standard_axes(1.0, 0.5, 0.5, 0.5, 1.0, 0.0);
    let n = builder.build_into(&mut report, &axes, 0, 1u32 << 0, None, None, None);

    assert_eq!(n, 17);
    // X = 65535 at bytes 0-1.
    assert_eq!(&report[0..2], &[0xFF, 0xFF]);
    // Y = 32767 (0.5 * 65535 truncated) at bytes 2-3.
    assert_eq!(&report[2..4], &[0xFF, 0x7F]);
    // Rx, Ry = 32767 at bytes 4-7.
    assert_eq!(&report[4..8], &[0xFF, 0x7F, 0xFF, 0x7F]);
    // Z = 1023 (10 bits at bit 64): byte 8 = 0xFF, byte 9 low 2 bits = 0b11.
    assert_eq!(report[8], 0xFF);
    assert_eq!(report[9] & 0x03, 0x03);
    // Rz = 0 (released): the low 2 bits of byte 10's region are clear.
    assert_eq!(report[10], 0x00);
    // Button A at descriptor button 0 → byte 12 bit 0.
    assert_eq!(report[12], 0x01);
    // Hat neutral (value 0) → null-state 0 in the low nibble of byte 14.
    assert_eq!(report[14] & 0x0F, 0x00);
}

#[test]
fn xbox_guide_routes_to_the_system_main_menu_field() {
    let profile = Profile::from_json(XBOX_SERIES_XS).expect("parse profile");
    let builder = ReportBuilder::parse(
        &profile.descriptor_bytes(),
        profile.axis_map_pairs().as_deref(),
        profile.button_map.clone(),
        profile.trigger_buttons_pair(),
        profile.preferred_report_id(),
    );
    let mut report = vec![0u8; builder.report_byte_size()];
    let axes = builder.standard_axes(0.5, 0.5, 0.5, 0.5, 0.0, 0.0);
    // Guide is HMButton bit 10; buttonMap maps it to -1 (dropped from the button
    // array) and the descriptor's System Main Menu field carries it instead.
    builder.build_into(&mut report, &axes, 0, 1u32 << 10, None, None, None);

    // System Main Menu at bit 120 = byte 15 bit 0.
    assert_eq!(
        report[15] & 0x01,
        0x01,
        "Guide should land in System Main Menu"
    );
    // And it must NOT smear into the regular button bytes (12-13).
    assert_eq!(report[12], 0x00);
    assert_eq!(report[13], 0x00);
}

#[test]
fn a_wheel_layout_routes_the_wheel_and_pedal_into_stick_and_trigger_slots() {
    // The Logitech Driving Force GT: wheel on X, accelerator pedal on Z. After
    // applying the layout, the wheel axis becomes LeftStickX and the accelerator
    // becomes LeftTrigger — so standard_axes maps them onto their descriptor
    // usages (X = 0x0130, Z = 0x0132), the effect ApplyLayoutSemantics produces.
    let profile = Profile::from_json(DRIVING_FORCE_GT).expect("parse profile");
    let mut builder = ReportBuilder::parse(
        &profile.descriptor_bytes(),
        profile.axis_map_pairs().as_deref(),
        profile.button_map.clone(),
        profile.trigger_buttons_pair(),
        profile.preferred_report_id(),
    );
    let layout = profile.layout.clone().expect("wheel profile has a layout");
    builder.apply_layout(&layout);

    // Distinctive values so a mis-routed slot is visible.
    let axes = builder.standard_axes(0.9, 0.5, 0.5, 0.5, 0.3, 0.0);
    assert_eq!(
        axes.get(&0x0130),
        Some(&0.9),
        "wheel (X) should be LeftStickX"
    );
    assert_eq!(
        axes.get(&0x0132),
        Some(&0.3),
        "accelerator (Z) should be LeftTrigger"
    );
}

#[test]
fn hat_octants_land_at_the_descriptor_positions() {
    let profile = Profile::from_json(XBOX_SERIES_XS).expect("parse profile");
    let builder = ReportBuilder::parse(
        &profile.descriptor_bytes(),
        profile.axis_map_pairs().as_deref(),
        profile.button_map.clone(),
        profile.trigger_buttons_pair(),
        profile.preferred_report_id(),
    );
    let axes = builder.standard_axes(0.5, 0.5, 0.5, 0.5, 0.0, 0.0);
    // Hat range is [1..8] (8 positions). Octant N=1 → logical_min+0 = 1;
    // NE=2 → 2; … NW=8 → 8. The nibble sits at bit 112 = byte 14 low nibble.
    for (octant, expected_nibble) in [(1, 1u8), (2, 2), (3, 3), (5, 5), (8, 8)] {
        let mut report = vec![0u8; builder.report_byte_size()];
        builder.build_into(&mut report, &axes, octant, 0, None, None, None);
        assert_eq!(
            report[14] & 0x0F,
            expected_nibble,
            "octant {octant} should write nibble {expected_nibble}"
        );
    }
}

// SPDX-License-Identifier: GPL-2.0-or-later

//! A HID report-descriptor parser — the field offsets a report is built into.
//!
//! Ported from HIDMaestro's `HidReportBuilder.ParseDescriptor`. This is the path
//! that serves **every profile without an `extendedReport`** (Xbox, generic HID,
//! USB Sony before arming) — 214 of 230 — where the report layout is expressed by
//! the device's own HID report descriptor rather than a vendor-blob field list.
//! It parses the descriptor bytes into a flat list of input [`InputField`]s with
//! their bit offsets, which [`super::report`] then classifies and packs.
//!
//! Fully data-driven: no controller-specific code, works with any descriptor.
//! Parsed once at controller creation, never on the frame path.
//!
//! Field offsets and sizes are kept as `i32` to mirror the C# arithmetic exactly
//! (a faithful transcription is safer than a re-derivation here); the buffer-index
//! sites in [`super::report`] cast to `usize`.

/// One input field the descriptor declares: a HID usage at a bit position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputField {
    pub usage_page: u16,
    pub usage: u16,
    pub bit_offset: i32,
    pub bit_size: i32,
    pub logical_min: i32,
    pub logical_max: i32,
    pub is_constant: bool,
    pub report_count: i32,
}

/// The parsed input report: which report id it is, its bit size, and its fields.
#[derive(Debug, Clone, Default)]
pub struct Descriptor {
    /// The selected input report id (0 when the descriptor declares no report id).
    pub input_report_id: u8,
    /// Total bits of the selected input report (excluding the report-id byte).
    pub input_report_bit_size: i32,
    pub input_fields: Vec<InputField>,
}

/// Parse a HID report descriptor. `preferred_report_id` is the report the profile
/// itself declares as its input report (issue #58), honored only if the descriptor
/// actually carries Input items for it; `0` selects by position.
pub fn parse(desc: &[u8], preferred_report_id: u8) -> Descriptor {
    // Parser state.
    let mut usage_page: u16 = 0;
    let mut usages: Vec<u16> = Vec::new();
    let mut usage_min: u16 = 0;
    let mut usage_max: u16 = 0;
    let mut report_size: i32 = 0;
    let mut report_count: i32 = 0;
    let mut logical_min: i32 = 0;
    let mut logical_max: i32 = 0;
    let mut report_id: u8 = 0;

    // Fields + running bit offset per input report id, in encounter order. The
    // Switch Pro BT descriptor declares vendor blobs before its parseable gamepad
    // report, so every input report is collected and one is chosen afterward.
    use std::collections::HashMap;
    let mut fields_by_report: HashMap<u8, Vec<InputField>> = HashMap::new();
    let mut bit_offset_by_report: HashMap<u8, i32> = HashMap::new();
    let mut report_order: Vec<u8> = Vec::new();

    let mut i = 0usize;
    while i < desc.len() {
        let prefix = desc[i];
        if prefix == 0xFE {
            // Long item — skipped, as HIDMaestro does.
            i += 3;
            continue;
        }

        let mut b_size = (prefix & 0x03) as usize;
        if b_size == 3 {
            b_size = 4;
        }
        let b_type = (prefix >> 2) & 0x03;
        let b_tag = (prefix >> 4) & 0x0F;

        let mut value: i32 = 0;
        if i + b_size < desc.len() {
            for j in 0..b_size {
                value |= (desc[i + 1 + j] as i32) << (8 * j);
            }
        }
        // Sign-extend for signed items (Logical Min, etc.); not for 4-byte items.
        let mut signed_value = value;
        if b_size > 0 && b_size < 4 && (value & (1 << (b_size * 8 - 1))) != 0 {
            signed_value |= ((0xFFFF_FFFFu32) << (b_size * 8)) as i32;
        }

        match b_type {
            0 => {
                // Main
                match b_tag {
                    8 => {
                        // Input
                        let is_constant = (value & 0x01) != 0;
                        let fields = fields_by_report.entry(report_id).or_insert_with(|| {
                            bit_offset_by_report.insert(report_id, 0);
                            report_order.push(report_id);
                            Vec::new()
                        });
                        let bit_offset = *bit_offset_by_report.get(&report_id).unwrap_or(&0);
                        if usage_min != 0 && usage_max != 0 {
                            // Button range.
                            for b in 0..report_count {
                                let mut u = usage_min.wrapping_add(b as u16);
                                if u > usage_max {
                                    u = usage_max;
                                }
                                fields.push(InputField {
                                    usage_page,
                                    usage: u,
                                    bit_offset: bit_offset + b * report_size,
                                    bit_size: report_size,
                                    logical_min,
                                    logical_max,
                                    is_constant,
                                    report_count,
                                });
                            }
                        } else {
                            for c in 0..report_count {
                                let u = usages.get(c as usize).copied().unwrap_or(0);
                                fields.push(InputField {
                                    usage_page,
                                    usage: u,
                                    bit_offset: bit_offset + c * report_size,
                                    bit_size: report_size,
                                    logical_min,
                                    logical_max,
                                    is_constant,
                                    report_count,
                                });
                            }
                        }
                        bit_offset_by_report
                            .insert(report_id, bit_offset + report_size * report_count);
                        usages.clear();
                        usage_min = 0;
                        usage_max = 0;
                    }
                    9 | 11 => {
                        // Output / Feature — different direction, skip.
                        usages.clear();
                        usage_min = 0;
                        usage_max = 0;
                    }
                    10 => {
                        // Collection — a usage before it is the collection's.
                        usages.clear();
                        usage_min = 0;
                        usage_max = 0;
                    }
                    12 => {} // End Collection
                    _ => {}
                }
            }
            1 => {
                // Global
                match b_tag {
                    0 => usage_page = value as u16,
                    1 => logical_min = signed_value,
                    2 => {
                        // Logical Max is unsigned when Logical Min is non-negative.
                        logical_max = if logical_min >= 0 && signed_value < 0 {
                            value
                        } else {
                            signed_value
                        };
                    }
                    7 => report_size = value,
                    8 => report_id = value as u8,
                    9 => report_count = value,
                    _ => {}
                }
            }
            2 => {
                // Local
                match b_tag {
                    0 => {
                        if b_size == 4 {
                            // Extended usage: low 16 = usage id, high 16 = page.
                            usage_page = ((value as u32) >> 16) as u16;
                            usages.push((value & 0xFFFF) as u16);
                        } else {
                            usages.push(value as u16);
                        }
                    }
                    1 => usage_min = value as u16,
                    2 => usage_max = value as u16,
                    _ => {}
                }
            }
            _ => {}
        }

        i += 1 + b_size;
    }

    // Select the layout report: the profile's declaration outranks position, but
    // only for a report the descriptor really declares Input items for; else the
    // first input report carrying a non-constant field on a gamepad-parseable page
    // (Generic Desktop 0x01, Simulation 0x02, Button 0x09); else the first report.
    let mut chosen_id: u8 = 0;
    let mut chosen = false;
    if preferred_report_id != 0 && report_order.contains(&preferred_report_id) {
        chosen_id = preferred_report_id;
        chosen = true;
    }
    if !chosen {
        for &id in &report_order {
            if let Some(fields) = fields_by_report.get(&id) {
                for f in fields {
                    if !f.is_constant
                        && (f.usage_page == 0x01 || f.usage_page == 0x02 || f.usage_page == 0x09)
                    {
                        chosen_id = id;
                        chosen = true;
                        break;
                    }
                }
            }
            if chosen {
                break;
            }
        }
    }
    if !chosen && !report_order.is_empty() {
        chosen_id = report_order[0];
        chosen = true;
    }

    let mut out = Descriptor::default();
    if chosen {
        out.input_report_id = chosen_id;
        if let Some(fields) = fields_by_report.remove(&chosen_id) {
            out.input_fields = fields;
        }
        out.input_report_bit_size = *bit_offset_by_report.get(&chosen_id).unwrap_or(&0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal gamepad: one report id 1, X+Y (8-bit each, signed-ish 0..255),
    // then 8 one-bit buttons.
    //
    //   05 01        Usage Page (Generic Desktop)
    //   09 05        Usage (Game Pad)
    //   a1 01        Collection (Application)
    //   85 01          Report ID (1)
    //   09 30          Usage (X)
    //   09 31          Usage (Y)
    //   15 00          Logical Min (0)
    //   26 ff 00       Logical Max (255)
    //   75 08          Report Size (8)
    //   95 02          Report Count (2)
    //   81 02          Input (Data,Var,Abs)
    //   05 09          Usage Page (Button)
    //   19 01          Usage Min (1)
    //   29 08          Usage Max (8)
    //   15 00          Logical Min (0)
    //   25 01          Logical Max (1)
    //   75 01          Report Size (1)
    //   95 08          Report Count (8)
    //   81 02          Input (Data,Var,Abs)
    //   c0           End Collection
    const GAMEPAD: &[u8] = &[
        0x05, 0x01, 0x09, 0x05, 0xa1, 0x01, 0x85, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x00, 0x26,
        0xff, 0x00, 0x75, 0x08, 0x95, 0x02, 0x81, 0x02, 0x05, 0x09, 0x19, 0x01, 0x29, 0x08, 0x15,
        0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0xc0,
    ];

    #[test]
    fn parses_axes_and_a_button_range() {
        let d = parse(GAMEPAD, 0);
        assert_eq!(d.input_report_id, 1);
        // 2 axes × 8 bits + 8 buttons × 1 bit = 24 bits.
        assert_eq!(d.input_report_bit_size, 24);
        assert_eq!(d.input_fields.len(), 10);

        // X at bit 0, Y at bit 8, both 8-bit, page Generic Desktop.
        let x = d.input_fields[0];
        assert_eq!(
            (x.usage_page, x.usage, x.bit_offset, x.bit_size),
            (0x01, 0x30, 0, 8)
        );
        assert_eq!((x.logical_min, x.logical_max), (0, 255));
        let y = d.input_fields[1];
        assert_eq!((y.usage, y.bit_offset), (0x31, 8));

        // Eight buttons, page 0x09, usages 1..8, at bits 16..23.
        for b in 0..8 {
            let f = d.input_fields[2 + b];
            assert_eq!(f.usage_page, 0x09);
            assert_eq!(f.usage, (b + 1) as u16);
            assert_eq!(f.bit_offset, 16 + b as i32);
            assert_eq!(f.bit_size, 1);
        }
    }

    #[test]
    fn logical_min_sign_extends() {
        // Report Size 8, Logical Min -128 (0x80), Logical Max 127.
        //   05 01 09 30 a1 01 09 30 15 80 25 7f 75 08 95 01 81 02 c0
        let desc: &[u8] = &[
            0x05, 0x01, 0x09, 0x30, 0xa1, 0x01, 0x09, 0x30, 0x15, 0x80, 0x25, 0x7f, 0x75, 0x08,
            0x95, 0x01, 0x81, 0x02, 0xc0,
        ];
        let d = parse(desc, 0);
        assert_eq!(d.input_report_id, 0); // no Report ID declared
        assert_eq!(d.input_fields.len(), 1);
        assert_eq!(d.input_fields[0].logical_min, -128);
        assert_eq!(d.input_fields[0].logical_max, 127);
    }

    #[test]
    fn a_constant_padding_field_is_marked() {
        // X (8 bit, data), then 8 bits of constant padding.
        //   05 01 09 30 a1 01 09 30 75 08 95 01 81 02  75 08 95 01 81 03  c0
        let desc: &[u8] = &[
            0x05, 0x01, 0x09, 0x30, 0xa1, 0x01, 0x09, 0x30, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02,
            0x75, 0x08, 0x95, 0x01, 0x81, 0x03, 0xc0,
        ];
        let d = parse(desc, 0);
        assert_eq!(d.input_fields.len(), 2);
        assert!(!d.input_fields[0].is_constant);
        assert!(d.input_fields[1].is_constant);
        assert_eq!(d.input_report_bit_size, 16);
    }
}

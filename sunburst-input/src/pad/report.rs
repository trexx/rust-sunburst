// SPDX-License-Identifier: GPL-2.0-or-later

//! The descriptor-driven report builder — packs a report from a parsed descriptor.
//!
//! Ported from HIDMaestro's `HidReportBuilder` (`ResolveSemantics`, `ApplyAxisMap`,
//! `BuildReportInto`, `WriteBits`). Given the fields [`super::hid`] parsed, this
//! classifies them into semantic slots (sticks, triggers, hat, buttons, …), then
//! packs a caller-supplied device state into the exact bit positions the profile's
//! HID descriptor declares. This is the legacy/HID report every non-`extendedReport`
//! profile emits (Xbox, generic HID, USB Sony before arming) and the report that
//! joy.cpl, DirectInput, WGI, browsers and Steam Input's HID reader consume.
//!
//! Axis values are `[0.0, 1.0]` normalised (0.5 = centre for signed axes), keyed by
//! the HID `(usage_page << 8) | usage` — HIDMaestro's `HMAxis`. Verified against
//! transcribed HIDMaestro probe assertions and hand-decoded descriptors; no
//! SHA-golden table exists for this path.

use std::collections::HashMap;

use super::hid::{self, InputField};

/// Axis values, keyed by `(usage_page << 8) | usage`. The caller reuses one map
/// across frames (this is the ~250 Hz input path, not the video frame path).
pub type Axes = HashMap<u16, f32>;

/// HIDMaestro `HMAxis` handles used here.
mod axis {
    pub const Z: u16 = 0x0132;
    pub const RZ: u16 = 0x0135;
}

/// Whether `(page << 8) | usage` is a recognised analog axis (HMAxis). Mirrors
/// HIDMaestro's `Enum.IsDefined(HMAxis, key)` gate on `AxisFields`.
fn is_known_axis(key: u16) -> bool {
    matches!(
        key,
        // Generic Desktop (0x01)
        0x0130..=0x0139 | 0x0140..=0x0146
        // Simulation Controls (0x02)
        | 0x02B0 | 0x02B1 | 0x02B2 | 0x02B5 | 0x02B6 | 0x02B8 | 0x02B9 | 0x02BA
        | 0x02BB | 0x02BE | 0x02BF | 0x02C3 | 0x02C4 | 0x02C5 | 0x02C6 | 0x02C7
        | 0x02C8 | 0x02C9 | 0x02CA | 0x02CB | 0x02CC | 0x02CD | 0x02CE | 0x02CF
        | 0x02D0
    )
}

/// The Guide button's HMButton bit — routed to the System Main Menu field on
/// descriptors that declare one (Xbox Series / One).
const GUIDE_BIT: u32 = 10;

/// A descriptor compiled to semantic slots, ready to pack reports.
#[derive(Debug, Clone, Default)]
pub struct ReportBuilder {
    pub input_report_id: u8,
    pub input_report_bit_size: i32,
    input_fields: Vec<InputField>,

    left_stick_x: Option<InputField>,
    left_stick_y: Option<InputField>,
    right_stick_x: Option<InputField>,
    right_stick_y: Option<InputField>,
    third_stick_x: Option<InputField>,
    third_stick_y: Option<InputField>,
    fourth_stick_x: Option<InputField>,
    fourth_stick_y: Option<InputField>,
    left_trigger: Option<InputField>,
    right_trigger: Option<InputField>,
    combined_trigger: Option<InputField>,
    hat_switch: Option<InputField>,
    system_main_menu: Option<InputField>,
    buttons: Vec<InputField>,
    vendor_button_bits: Vec<InputField>,
    axis_fields: HashMap<u16, InputField>,

    /// `HMButton` bit → descriptor button index (or vendor bit past `buttons`).
    button_map: Option<Vec<i32>>,
    /// `[left_trigger_button, right_trigger_button]` descriptor indices.
    trigger_buttons: Option<[i32; 2]>,
    /// Canonical trigger axis handles for the combined-Z synthesis.
    canonical_lt: u16,
    canonical_rt: u16,
}

impl ReportBuilder {
    /// Parse a descriptor and resolve its semantics, applying the profile's
    /// `axis_map` (bare usage → role), `button_map` and `trigger_buttons`.
    pub fn parse(
        descriptor: &[u8],
        axis_map: Option<&[(u16, String)]>,
        button_map: Option<Vec<i32>>,
        trigger_buttons: Option<[i32; 2]>,
        preferred_report_id: u8,
    ) -> ReportBuilder {
        let parsed = hid::parse(descriptor, preferred_report_id);
        let mut b = ReportBuilder {
            input_report_id: parsed.input_report_id,
            input_report_bit_size: parsed.input_report_bit_size,
            input_fields: parsed.input_fields,
            button_map,
            trigger_buttons,
            canonical_lt: resolve_canonical_axis(axis_map, "lefttrigger", axis::Z),
            canonical_rt: resolve_canonical_axis(axis_map, "righttrigger", axis::RZ),
            ..Default::default()
        };
        b.resolve_semantics();
        if let Some(map) = axis_map {
            b.apply_axis_map(map);
        }
        b
    }

    /// Bytes of the input report, including the report-id byte when present.
    pub fn report_byte_size(&self) -> usize {
        (self.input_report_bit_size as usize).div_ceil(8) + usize::from(self.input_report_id != 0)
    }

    fn claim_right_stick_x(&mut self, f: InputField) {
        if self.right_stick_x.is_none() {
            self.right_stick_x = Some(f);
        } else if self.third_stick_x.is_none() {
            self.third_stick_x = Some(f);
        } else if self.fourth_stick_x.is_none() {
            self.fourth_stick_x = Some(f);
        }
    }
    fn claim_right_stick_y(&mut self, f: InputField) {
        if self.right_stick_y.is_none() {
            self.right_stick_y = Some(f);
        } else if self.third_stick_y.is_none() {
            self.third_stick_y = Some(f);
        } else if self.fourth_stick_y.is_none() {
            self.fourth_stick_y = Some(f);
        }
    }
    fn claim_left_trigger(&mut self, f: InputField) {
        if self.left_trigger.is_none() {
            self.left_trigger = Some(f);
        } else if self.right_trigger.is_none() {
            self.right_trigger = Some(f);
        }
    }
    fn claim_right_trigger(&mut self, f: InputField) {
        if self.right_trigger.is_none() {
            self.right_trigger = Some(f);
        } else if self.left_trigger.is_none() {
            self.left_trigger = Some(f);
        }
    }

    fn resolve_semantics(&mut self) {
        // Prescan: is Z/Rz the right stick (not triggers)? See HIDMaestro #5/#22/#27.
        let (mut has_rx_or_ry, mut has_z, mut has_rz, mut z_rz_sticks_by_count) =
            (false, false, false, false);
        for f in &self.input_fields {
            if f.is_constant || f.usage_page != 0x01 {
                continue;
            }
            match f.usage {
                0x33 | 0x34 => has_rx_or_ry = true,
                0x32 => {
                    has_z = true;
                    if f.report_count >= 2 {
                        z_rz_sticks_by_count = true;
                    }
                }
                0x35 => {
                    has_rz = true;
                    if f.report_count >= 2 {
                        z_rz_sticks_by_count = true;
                    }
                }
                _ => {}
            }
        }
        let four_axis_dinput = !has_rx_or_ry && has_z && has_rz;
        let z_rz_are_sticks = four_axis_dinput || z_rz_sticks_by_count;

        // A Generic-Desktop field with Report Count 1 and unsigned range from 0 is
        // a trigger (matches AddTrigger); wins over the right-stick fallback (#22).
        fn looks_like_trigger(f: &InputField) -> bool {
            f.report_count == 1 && f.logical_min == 0
        }

        let fields = self.input_fields.clone();
        for f in &fields {
            if f.is_constant {
                continue;
            }

            // Catalog recognized analog usages (skip Hat 0x0139).
            if (f.usage_page == 0x01 || f.usage_page == 0x02)
                && !(f.usage_page == 0x01 && f.usage == 0x39)
            {
                let key = (f.usage_page << 8) | f.usage;
                if is_known_axis(key) {
                    self.axis_fields.entry(key).or_insert(*f);
                }
            }

            if f.usage_page == 0x01 {
                match f.usage {
                    0x30 if self.left_stick_x.is_none() => self.left_stick_x = Some(*f),
                    0x31 if self.left_stick_y.is_none() => self.left_stick_y = Some(*f),
                    0x32 => {
                        // Z
                        if looks_like_trigger(f) || !z_rz_are_sticks {
                            self.claim_left_trigger(*f);
                        } else {
                            self.claim_right_stick_x(*f);
                        }
                    }
                    0x33 => {
                        // Rx
                        if looks_like_trigger(f) {
                            self.claim_left_trigger(*f);
                        } else {
                            self.claim_right_stick_x(*f);
                        }
                    }
                    0x34 => {
                        // Ry
                        if looks_like_trigger(f) {
                            self.claim_right_trigger(*f);
                        } else {
                            self.claim_right_stick_y(*f);
                        }
                    }
                    0x35 => {
                        // Rz
                        if looks_like_trigger(f) || !z_rz_are_sticks {
                            self.claim_right_trigger(*f);
                        } else {
                            self.claim_right_stick_y(*f);
                        }
                    }
                    0x36 => {
                        // Slider
                        if looks_like_trigger(f) {
                            self.claim_left_trigger(*f);
                        } else {
                            self.claim_right_stick_x(*f);
                        }
                    }
                    0x37 => {
                        // Dial
                        if looks_like_trigger(f) {
                            self.claim_right_trigger(*f);
                        } else {
                            self.claim_right_stick_y(*f);
                        }
                    }
                    0x39 if self.hat_switch.is_none() => self.hat_switch = Some(*f),
                    0x85 if self.system_main_menu.is_none() => self.system_main_menu = Some(*f),
                    0x40 => {
                        // Vx — hidden separate LT for WGI; save Z as combined first.
                        if self.combined_trigger.is_none() {
                            self.combined_trigger = self.left_trigger;
                        }
                        self.left_trigger = Some(*f);
                    }
                    0x41 => self.right_trigger = Some(*f), // Vy
                    _ => {}
                }
            } else if f.usage_page == 0x02 {
                match f.usage {
                    // Accelerator / Rudder → right trigger; Brake / Throttle → left.
                    0xC4 | 0xBA if self.right_trigger.is_none() => self.right_trigger = Some(*f),
                    0xC5 | 0xBB if self.left_trigger.is_none() => self.left_trigger = Some(*f),
                    _ => {}
                }
            } else if f.usage_page == 0x09 || f.usage_page == 0x0C {
                // Button / Consumer (Share, Record, …).
                self.buttons.push(*f);
            }
        }

        // The vendor 1-bit run contiguous with the button array (#48).
        if let Some(last) = self.buttons.last() {
            let mut run_end = last.bit_offset + last.bit_size;
            for f in &fields {
                if f.usage_page < 0xFF00 || f.is_constant || f.bit_size != 1 {
                    continue;
                }
                if f.bit_offset != run_end {
                    continue;
                }
                self.vendor_button_bits.push(*f);
                run_end += 1;
            }
        }
    }

    fn apply_axis_map(&mut self, map: &[(u16, String)]) {
        // usage (page 0x01) → field.
        let mut field_by_usage: HashMap<u16, InputField> = HashMap::new();
        for f in &self.input_fields {
            if f.is_constant || f.usage_page != 0x01 {
                continue;
            }
            field_by_usage.entry(f.usage).or_insert(*f);
        }

        // Scrub every field the map will reassign out of all slots first (#124).
        let mut assigned: Vec<InputField> = Vec::new();
        for (usage, _) in map {
            if let Some(f) = field_by_usage.get(usage) {
                assigned.push(*f);
            }
        }
        let taken = |slot: &Option<InputField>| slot.is_some_and(|s| assigned.contains(&s));
        for slot in [
            &mut self.left_stick_x,
            &mut self.left_stick_y,
            &mut self.right_stick_x,
            &mut self.right_stick_y,
            &mut self.third_stick_x,
            &mut self.third_stick_y,
            &mut self.fourth_stick_x,
            &mut self.fourth_stick_y,
            &mut self.left_trigger,
            &mut self.right_trigger,
        ] {
            if taken(slot) {
                *slot = None;
            }
        }

        // Apply the overrides.
        for (usage, role) in map {
            let Some(field) = field_by_usage.get(usage).copied() else {
                continue;
            };
            match role.to_ascii_lowercase().as_str() {
                "leftstickx" => self.left_stick_x = Some(field),
                "leftsticky" => self.left_stick_y = Some(field),
                "rightstickx" => self.right_stick_x = Some(field),
                "rightsticky" => self.right_stick_y = Some(field),
                "lefttrigger" => self.left_trigger = Some(field),
                "righttrigger" => self.right_trigger = Some(field),
                _ => {}
            }
        }
    }

    fn find_axis(&self, name: &str) -> Option<InputField> {
        super::layout::axis_handle(name).and_then(|h| self.axis_fields.get(&h).copied())
    }
    fn axis_field(&self, handle: u16) -> Option<InputField> {
        self.axis_fields.get(&handle).copied()
    }

    /// Apply an authored `layout`, routing a non-gamepad device's axes into the
    /// classic stick/trigger slots (`ApplyLayoutSemantics`). A gamepad layout is
    /// author-aligned and left untouched. Call after [`parse`](Self::parse).
    pub fn apply_layout(&mut self, layout: &super::layout::Layout) {
        use super::layout::LayoutKind;
        match layout.kind() {
            LayoutKind::Gamepad | LayoutKind::Unspecified => {}
            LayoutKind::Joystick | LayoutKind::FlightStick => {
                let sx = layout.stick.as_ref().and_then(|s| self.find_axis(&s.x));
                let sy = layout
                    .stick
                    .as_ref()
                    .and_then(|s| s.y.as_deref())
                    .and_then(|y| self.find_axis(y));
                let lt = layout
                    .throttle
                    .as_ref()
                    .and_then(|t| self.find_axis(&t.axis));
                let rt = layout.rudder.as_ref().and_then(|r| self.find_axis(&r.axis));
                self.left_stick_x = sx;
                self.left_stick_y = sy;
                self.right_stick_x = None;
                self.right_stick_y = None;
                self.left_trigger = lt;
                self.right_trigger = rt;
                self.combined_trigger = None;
            }
            LayoutKind::Hotas => {
                let sx = layout.stick.as_ref().and_then(|s| self.find_axis(&s.x));
                let sy = layout
                    .stick
                    .as_ref()
                    .and_then(|s| s.y.as_deref())
                    .and_then(|y| self.find_axis(y));
                let lt = layout
                    .throttle_primary
                    .as_ref()
                    .and_then(|t| self.find_axis(&t.axis));
                let rt = layout
                    .stick_rudder
                    .as_ref()
                    .and_then(|r| self.find_axis(&r.axis));
                self.left_stick_x = sx;
                self.left_stick_y = sy;
                self.right_stick_x = None;
                self.right_stick_y = None;
                self.left_trigger = lt;
                self.right_trigger = rt;
                self.combined_trigger = None;
            }
            LayoutKind::Wheel => {
                let sx = layout.wheel.as_ref().and_then(|w| self.find_axis(&w.axis));
                let sy = layout
                    .pedal_axis(&["clutch"])
                    .and_then(|h| self.axis_field(h));
                let lt = layout
                    .pedal_axis(&["accelerator", "throttle"])
                    .and_then(|h| self.axis_field(h));
                let rt = layout
                    .pedal_axis(&["brake"])
                    .and_then(|h| self.axis_field(h));
                self.left_stick_x = sx;
                self.left_stick_y = sy;
                self.right_stick_x = None;
                self.right_stick_y = None;
                self.left_trigger = lt;
                self.right_trigger = rt;
                self.combined_trigger = None;
            }
            LayoutKind::Pedals => {
                let lt = layout
                    .pedal_axis(&["accelerator", "throttle"])
                    .and_then(|h| self.axis_field(h));
                let rt = layout
                    .pedal_axis(&["brake"])
                    .and_then(|h| self.axis_field(h));
                self.left_stick_x = None;
                self.left_stick_y = None;
                self.right_stick_x = None;
                self.right_stick_y = None;
                self.left_trigger = lt;
                self.right_trigger = rt;
                self.combined_trigger = None;
            }
            LayoutKind::Handbrake | LayoutKind::SingleAxisAccessory => {
                let lt = layout.axis.as_deref().and_then(|a| self.find_axis(a));
                self.left_stick_x = None;
                self.left_stick_y = None;
                self.right_stick_x = None;
                self.right_stick_y = None;
                self.left_trigger = lt;
                self.right_trigger = None;
                self.combined_trigger = None;
            }
            // Devices that don't fit the simple stick/trigger framework.
            LayoutKind::ArcadeStick
            | LayoutKind::DancePad
            | LayoutKind::Guitar
            | LayoutKind::MotionWand
            | LayoutKind::Remote
            | LayoutKind::Shifter
            | LayoutKind::ControllerAdapter => {
                self.left_stick_x = None;
                self.left_stick_y = None;
                self.right_stick_x = None;
                self.right_stick_y = None;
                self.left_trigger = None;
                self.right_trigger = None;
                self.combined_trigger = None;
            }
        }
    }

    /// Pack a report into `report` (≥ [`report_byte_size`]). Returns bytes written.
    #[allow(clippy::too_many_arguments)]
    pub fn build_into(
        &self,
        report: &mut [u8],
        axes: &Axes,
        hat_value: i32,
        button_mask: u32,
        hat_degrees: Option<f32>,
        hat_hundredths: Option<i32>,
        hat_raw: Option<u16>,
    ) -> usize {
        let size = self.report_byte_size();
        report[..size].fill(0);
        if self.input_report_id != 0 {
            report[0] = self.input_report_id;
        }
        let id_offset = if self.input_report_id != 0 { 8 } else { 0 };

        let write_field = |report: &mut [u8], field: Option<InputField>, normalized: f64| {
            let Some(field) = field else { return };
            let span = (field.logical_max - field.logical_min) as f64;
            let raw = (normalized * span + field.logical_min as f64) as i32;
            let raw = raw.clamp(field.logical_min, field.logical_max);
            write_bits(report, field.bit_offset + id_offset, field.bit_size, raw);
        };
        let get_axis = |key: u16, def: f64| -> f64 {
            axes.get(&key)
                .map(|v| (*v as f64).clamp(0.0, 1.0))
                .unwrap_or(def)
        };

        // Per-axis pass: every declared analog input from the dict.
        for (key, field) in &self.axis_fields {
            let def = if field.logical_min < 0 { 0.5 } else { 0.0 };
            write_field(report, Some(*field), get_axis(*key, def));
        }

        // Combined-Z trigger synthesis (Xbox 360 Vx/Vy). dinput sees combined Z;
        // XInput/WGI see the separate Vx/Vy.
        if let (Some(ct), Some(lt), Some(rt)) =
            (self.combined_trigger, self.left_trigger, self.right_trigger)
        {
            let lt_key = (lt.usage_page << 8) | lt.usage;
            let rt_key = (rt.usage_page << 8) | rt.usage;
            let get_trigger = |canonical: u16, field_key: u16| -> f64 {
                if let Some(v) = axes.get(&canonical) {
                    return (*v as f64).clamp(0.0, 1.0);
                }
                if let Some(v) = axes.get(&field_key) {
                    return (*v as f64).clamp(0.0, 1.0);
                }
                0.0
            };
            let lt_v = get_trigger(self.canonical_lt, lt_key);
            let rt_v = get_trigger(self.canonical_rt, rt_key);
            let combined = (0.5 + (rt_v - lt_v) * 0.5).clamp(0.0, 1.0);
            write_field(report, Some(ct), combined);
            write_field(report, Some(lt), lt_v);
            write_field(report, Some(rt), rt_v);
        }

        // Hat: priority chain degrees > hundredths > raw > octant > neutral null.
        if let Some(hat) = self.hat_switch {
            let range = hat.logical_max - hat.logical_min + 1;
            let hat_written = if let Some(deg) = hat_degrees {
                let a = ((deg % 360.0) + 360.0) % 360.0;
                let idx = (a / 360.0 * range as f32).round() as i32 % range;
                hat.logical_min + idx
            } else if let Some(h) = hat_hundredths {
                let v = ((h % 36000) + 36000) % 36000;
                let idx = (v as i64 * range as i64 / 36000) as i32;
                hat.logical_min + idx
            } else if let Some(r) = hat_raw {
                (r as i32).clamp(hat.logical_min, hat.logical_max)
            } else if hat_value == 0 {
                // Neutral null-state: value outside the logical range.
                if hat.logical_min == 0 {
                    hat.logical_max + 1
                } else {
                    0
                }
            } else {
                // Octant 1-8 scaled into the descriptor's range.
                let octant_idx = (hat_value - 1) * range / 8;
                hat.logical_min + octant_idx
            };
            write_bits(
                report,
                hat.bit_offset + id_offset,
                hat.bit_size,
                hat_written,
            );
        }

        // Trigger-to-button (DS4/DualSense L2/R2 as digital buttons).
        if let (Some([lt_btn, rt_btn]), Some(lt), Some(rt)) =
            (self.trigger_buttons, self.left_trigger, self.right_trigger)
        {
            let lt_key = (lt.usage_page << 8) | lt.usage;
            let rt_key = (rt.usage_page << 8) | rt.usage;
            let lt_v = get_axis(lt_key, 0.0);
            let rt_v = get_axis(rt_key, 0.0);
            if lt_v > 0.0 && lt_btn >= 0 && (lt_btn as usize) < self.buttons.len() {
                let b = self.buttons[lt_btn as usize];
                write_bits(report, b.bit_offset + id_offset, b.bit_size, 1);
            }
            if rt_v > 0.0 && rt_btn >= 0 && (rt_btn as usize) < self.buttons.len() {
                let b = self.buttons[rt_btn as usize];
                write_bits(report, b.bit_offset + id_offset, b.bit_size, 1);
            }
        }

        // Guide → System Main Menu (Xbox Series/One).
        let mut guide_routed = false;
        if let Some(sm) = self.system_main_menu
            && (button_mask >> GUIDE_BIT) & 1 != 0
        {
            write_bits(report, sm.bit_offset + id_offset, sm.bit_size, 1);
            guide_routed = true;
        }

        // Button packing with optional remap + vendor-bit run.
        let mut mask = button_mask;
        if guide_routed {
            mask &= !(1u32 << GUIDE_BIT);
        }
        while mask != 0 {
            let b = mask.trailing_zeros() as i32;
            mask &= mask - 1;
            let mapped = self
                .button_map
                .as_ref()
                .is_some_and(|m| (b as usize) < m.len());
            let desc_btn = if mapped {
                self.button_map.as_ref().unwrap()[b as usize]
            } else {
                b
            };
            if desc_btn >= 0 && (desc_btn as usize) < self.buttons.len() {
                let bf = self.buttons[desc_btn as usize];
                write_bits(report, bf.bit_offset + id_offset, bf.bit_size, 1);
            } else if mapped
                && desc_btn >= self.buttons.len() as i32
                && ((desc_btn - self.buttons.len() as i32) as usize) < self.vendor_button_bits.len()
            {
                let vb = self.vendor_button_bits[(desc_btn - self.buttons.len() as i32) as usize];
                write_bits(report, vb.bit_offset + id_offset, vb.bit_size, 1);
            }
        }

        size
    }

    /// Build an axes dict from the canonical 6-slot convention, keyed by each
    /// resolved slot's HID usage. For tests and one-shot frames.
    pub fn standard_axes(
        &self,
        left_x: f32,
        left_y: f32,
        right_x: f32,
        right_y: f32,
        left_trigger: f32,
        right_trigger: f32,
    ) -> Axes {
        let mut d = Axes::new();
        let key = |f: InputField| (f.usage_page << 8) | f.usage;
        if let Some(f) = self.left_stick_x {
            d.insert(key(f), left_x);
        }
        if let Some(f) = self.left_stick_y {
            d.insert(key(f), left_y);
        }
        if let Some(f) = self.right_stick_x {
            d.insert(key(f), right_x);
        }
        if let Some(f) = self.right_stick_y {
            d.insert(key(f), right_y);
        }
        if let Some(f) = self.left_trigger {
            d.insert(key(f), left_trigger);
        }
        if let Some(f) = self.right_trigger {
            d.insert(key(f), right_trigger);
        }
        d
    }
}

/// Resolve which HID usage a trigger role canonically lives on, from the profile's
/// axis map, else the default. Last match wins on duplicate roles (#34).
fn resolve_canonical_axis(axis_map: Option<&[(u16, String)]>, role: &str, default: u16) -> u16 {
    let Some(map) = axis_map else { return default };
    let mut resolved = default;
    for (usage, value) in map {
        if !value.eq_ignore_ascii_case(role) {
            continue;
        }
        let mut u = *usage;
        if u <= 0xFF {
            u |= 0x0100;
        }
        resolved = u;
    }
    resolved
}

/// Write `value` into `report` at a bit offset/size. Byte-aligned fast path, then a
/// bit-by-bit fallback for oddly-aligned button bitmaps.
fn write_bits(report: &mut [u8], bit_offset: i32, bit_size: i32, value: i32) {
    if bit_offset < 0 || bit_size <= 0 {
        return;
    }
    if bit_offset & 7 == 0 && bit_size & 7 == 0 {
        let byte_idx = (bit_offset >> 3) as usize;
        let byte_cnt = (bit_size >> 3) as usize;
        let mut v = value as u32;
        for i in 0..byte_cnt {
            if byte_idx + i >= report.len() {
                break;
            }
            report[byte_idx + i] = (v & 0xFF) as u8;
            v >>= 8;
        }
        return;
    }
    for b in 0..bit_size {
        let bit = (value >> b) & 1;
        let byte_idx = ((bit_offset + b) >> 3) as usize;
        let bit_idx = (bit_offset + b) & 7;
        if byte_idx < report.len() {
            if bit != 0 {
                report[byte_idx] |= 1u8 << bit_idx;
            } else {
                report[byte_idx] &= !(1u8 << bit_idx);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Same minimal gamepad as the hid tests: report id 1, X/Y 8-bit (0..255),
    // 8 buttons.
    const GAMEPAD: &[u8] = &[
        0x05, 0x01, 0x09, 0x05, 0xa1, 0x01, 0x85, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x00, 0x26,
        0xff, 0x00, 0x75, 0x08, 0x95, 0x02, 0x81, 0x02, 0x05, 0x09, 0x19, 0x01, 0x29, 0x08, 0x15,
        0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0xc0,
    ];

    #[test]
    fn classifies_the_minimal_gamepad() {
        let b = ReportBuilder::parse(GAMEPAD, None, None, None, 0);
        assert_eq!(b.input_report_id, 1);
        assert!(b.left_stick_x.is_some());
        assert!(b.left_stick_y.is_some());
        assert_eq!(b.buttons.len(), 8);
        // report id byte + 3 payload bytes (24 bits).
        assert_eq!(b.report_byte_size(), 4);
    }

    #[test]
    fn packs_sticks_and_buttons_at_their_offsets() {
        let b = ReportBuilder::parse(GAMEPAD, None, None, None, 0);
        let mut report = vec![0u8; b.report_byte_size()];

        // Left stick full-right (1.0 → 255), full-down-Y (1.0 → 255); buttons A(bit0)
        // and X(bit2) held. A→descriptor button 0, X→descriptor button 2 (identity).
        let axes = b.standard_axes(1.0, 1.0, 0.5, 0.5, 0.0, 0.0);
        let button_mask = (1u32 << 0) | (1u32 << 2);
        let n = b.build_into(&mut report, &axes, 0, button_mask, None, None, None);

        assert_eq!(n, 4);
        assert_eq!(report[0], 0x01, "report id");
        assert_eq!(report[1], 255, "X at byte 1");
        assert_eq!(report[2], 255, "Y at byte 2");
        // Buttons at byte 3: bit0 (A) + bit2 (X) = 0b0000_0101.
        assert_eq!(report[3], 0b0000_0101);
    }

    #[test]
    fn a_centered_stick_writes_the_midpoint() {
        let b = ReportBuilder::parse(GAMEPAD, None, None, None, 0);
        let mut report = vec![0u8; b.report_byte_size()];
        let axes = b.standard_axes(0.5, 0.5, 0.5, 0.5, 0.0, 0.0);
        b.build_into(&mut report, &axes, 0, 0, None, None, None);
        // 0.5 * 255 = 127.5 → truncated to 127 (C# (int) cast truncates).
        assert_eq!(report[1], 127);
        assert_eq!(report[2], 127);
    }

    #[test]
    fn a_button_map_relocates_a_button() {
        // Map HMButton bit 0 (A) → descriptor button 3.
        let b = ReportBuilder::parse(GAMEPAD, None, Some(vec![3]), None, 0);
        let mut report = vec![0u8; b.report_byte_size()];
        let axes = b.standard_axes(0.5, 0.5, 0.5, 0.5, 0.0, 0.0);
        b.build_into(&mut report, &axes, 0, 1u32 << 0, None, None, None);
        // Bit 0 (A) lands at descriptor button 3 → byte 3 bit 3.
        assert_eq!(report[3], 0b0000_1000);
    }
}

// SPDX-License-Identifier: GPL-2.0-or-later

//! A controller profile — HIDMaestro's JSON, the source of truth for a pad.
//!
//! A profile carries the device identity, its raw HID `descriptor`, and either an
//! `extendedReport` (the vendor-blob path, [`super::codec`]) or a
//! `descriptor` + `axisMap`/`buttonMap`/`triggerButtons` the descriptor builder
//! ([`super::report`]) consumes. This module deserialises it and exposes the
//! pieces each report path needs, so a controller is added by vendoring its JSON.
//!
//! Parsed once at controller creation, never on the frame path.

use std::collections::HashMap;

use serde::Deserialize;

use super::spec::ReportSpec;

/// A HIDMaestro profile. Only the fields the report paths need are read; serde
/// ignores the rest (`productString`, `notes`, `layout` metadata, …).
#[derive(Debug, Clone, Deserialize)]
pub struct Profile {
    pub id: String,
    /// Human-readable device name, shown in Device Manager / joy.cpl when the
    /// virtual pad is created. Optional in the JSON; falls back to `id`.
    #[serde(rename = "productString", default)]
    pub product_string: Option<String>,
    /// USB vendor id, `"0x045E"` etc.
    #[serde(default)]
    pub vid: Option<String>,
    /// USB product id.
    #[serde(default)]
    pub pid: Option<String>,
    /// Alternate PID the *driver* matches on (xinputhid's INF wants `0x02FF`);
    /// apps still read the real `pid` via HID attributes.
    #[serde(rename = "driverPid", default)]
    pub driver_pid: Option<String>,
    /// `"xinputhid"` / `"xusb22"` route the pad through an upper-filter companion
    /// (Xbox Series family); absent means a plain HID device.
    #[serde(rename = "driverMode", default)]
    pub driver_mode: Option<String>,
    /// Raw HID report descriptor, as a hex string.
    #[serde(default)]
    pub descriptor: String,
    /// Bare-usage → semantic-role overrides (`"0x32"` → `"leftTrigger"`).
    #[serde(rename = "axisMap", default)]
    pub axis_map: Option<HashMap<String, String>>,
    /// HMButton bit → descriptor button index (`-1` = this pad lacks the control).
    #[serde(rename = "buttonMap", default)]
    pub button_map: Option<Vec<i32>>,
    /// `[left_trigger_button, right_trigger_button]` descriptor indices (DS4/DS5).
    #[serde(rename = "triggerButtons", default)]
    pub trigger_buttons: Option<Vec<i32>>,
    /// The vendor-blob input report, when the profile declares one.
    #[serde(rename = "extendedReport", default)]
    pub extended_report: Option<ReportSpec>,
    /// The vendor-blob output report, when the profile declares one.
    #[serde(rename = "extendedOutputReport", default)]
    pub extended_output_report: Option<ReportSpec>,
    /// The authored device layout (wheel / HOTAS / pedals role assignment).
    #[serde(default)]
    pub layout: Option<super::layout::Layout>,
}

impl Profile {
    /// Parse a profile from its JSON. Tolerates a leading UTF-8 BOM, which some of
    /// HIDMaestro's vendored profiles carry (e.g. `nintendo/switch-pro.json`).
    pub fn from_json(json: &str) -> Result<Profile, serde_json::Error> {
        serde_json::from_str(json.strip_prefix('\u{FEFF}').unwrap_or(json))
    }

    /// The HID descriptor bytes, decoded from the hex string. Empty on malformed
    /// input (odd length / non-hex) — a profile with no usable descriptor.
    pub fn descriptor_bytes(&self) -> Vec<u8> {
        let s = self.descriptor.trim();
        if !s.len().is_multiple_of(2) {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(s.len() / 2);
        let bytes = s.as_bytes();
        let mut i = 0;
        while i + 1 < bytes.len() {
            let hi = (bytes[i] as char).to_digit(16);
            let lo = (bytes[i + 1] as char).to_digit(16);
            match (hi, lo) {
                (Some(h), Some(l)) => out.push((h * 16 + l) as u8),
                _ => return Vec::new(),
            }
            i += 2;
        }
        out
    }

    /// The axis map as `(bare_usage, role)` pairs for [`super::report::ReportBuilder`].
    pub fn axis_map_pairs(&self) -> Option<Vec<(u16, String)>> {
        let map = self.axis_map.as_ref()?;
        let mut pairs: Vec<(u16, String)> = map
            .iter()
            .filter_map(|(k, v)| parse_usage(k).map(|u| (u, v.clone())))
            .collect();
        // Deterministic order so "last match wins" (HIDMaestro #34) is stable.
        pairs.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        Some(pairs)
    }

    /// The name to show for the created device: the profile's `productString`
    /// when it has one, else its `id`.
    pub fn display_name(&self) -> &str {
        self.product_string.as_deref().unwrap_or(&self.id)
    }

    /// The USB vendor id, decoded.
    pub fn vid_u16(&self) -> Option<u16> {
        self.vid.as_deref().and_then(parse_usage)
    }

    /// The USB product id, decoded.
    pub fn pid_u16(&self) -> Option<u16> {
        self.pid.as_deref().and_then(parse_usage)
    }

    /// The PID used to form the hardware ID the driver INF matches — `driverPid`
    /// when the profile overrides it (xinputhid), else the real `pid`.
    pub fn driver_hw_pid(&self) -> Option<u16> {
        self.driver_pid
            .as_deref()
            .and_then(parse_usage)
            .or_else(|| self.pid_u16())
    }

    /// Whether this pad routes through the xinputhid / xusb22 upper-filter path
    /// (Xbox Series) rather than a plain HID node.
    pub fn uses_upper_filter(&self) -> bool {
        matches!(
            self.driver_mode.as_deref(),
            Some("xinputhid") | Some("xusb22")
        )
    }

    /// Whether this pad needs the XUSB companion (the Xbox 360 wired family):
    /// Microsoft VID and not an upper-filter pad.
    pub fn requires_xusb_companion(&self) -> bool {
        self.vid_u16() == Some(0x045E) && !self.uses_upper_filter()
    }

    /// `triggerButtons` as a fixed pair, when it has at least two entries.
    pub fn trigger_buttons_pair(&self) -> Option<[i32; 2]> {
        let tb = self.trigger_buttons.as_ref()?;
        (tb.len() >= 2).then(|| [tb[0], tb[1]])
    }

    /// The report id to prefer during descriptor parse: a profile that streams its
    /// vendor report from power-on names its own report; otherwise select by
    /// position (issue #58).
    pub fn preferred_report_id(&self) -> u8 {
        match &self.extended_report {
            Some(er) if er.always_armed == Some(true) => er.report_id_byte(),
            _ => 0,
        }
    }
}

/// Parse a `"0x32"` / `"50"` usage code.
fn parse_usage(s: &str) -> Option<u16> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u16::from_str_radix(hex, 16).ok()
    } else {
        s.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_descriptor_decodes() {
        let p = Profile::from_json(r#"{"id":"t","descriptor":"05010905"}"#).unwrap();
        assert_eq!(p.descriptor_bytes(), vec![0x05, 0x01, 0x09, 0x05]);
    }

    #[test]
    fn a_malformed_descriptor_is_empty_not_a_panic() {
        let odd = Profile::from_json(r#"{"id":"t","descriptor":"050109050"}"#).unwrap();
        assert!(odd.descriptor_bytes().is_empty());
        let bad = Profile::from_json(r#"{"id":"t","descriptor":"05zz"}"#).unwrap();
        assert!(bad.descriptor_bytes().is_empty());
    }

    #[test]
    fn axis_map_pairs_parse_and_order() {
        let p = Profile::from_json(
            r#"{"id":"t","descriptor":"","axisMap":{"0x35":"rightTrigger","0x33":"leftTrigger"}}"#,
        )
        .unwrap();
        assert_eq!(
            p.axis_map_pairs().unwrap(),
            vec![(0x33, "leftTrigger".into()), (0x35, "rightTrigger".into())]
        );
    }

    #[test]
    fn preferred_report_id_only_for_always_armed() {
        let armed = Profile::from_json(
            r#"{"id":"t","descriptor":"","extendedReport":{"reportId":"0x30","size":64,"fields":[],"alwaysArmed":true}}"#,
        )
        .unwrap();
        assert_eq!(armed.preferred_report_id(), 0x30);
        let on_demand = Profile::from_json(
            r#"{"id":"t","descriptor":"","extendedReport":{"reportId":"0x31","size":78,"fields":[]}}"#,
        )
        .unwrap();
        assert_eq!(on_demand.preferred_report_id(), 0);
    }
}

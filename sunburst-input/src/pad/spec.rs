// SPDX-License-Identifier: GPL-2.0-or-later

//! An `extendedReport` / `extendedOutputReport` spec, parsed from a HIDMaestro
//! profile's JSON.
//!
//! This is the data half of "all 234 profiles are data + one generic codec": the
//! JSON declares, per field, a wire type, a semantic source, a byte offset and
//! any packing parameters, and the codec in [`super::codec`] walks it. The field
//! names here are exactly HIDMaestro's, so a profile JSON is consumed verbatim as
//! the single source of truth — adding a controller later is copying its JSON,
//! never writing code.
//!
//! Parsed once at controller creation, never on the frame path.

use serde::Deserialize;

/// One `extendedReport` (input) or `extendedOutputReport` (output) block.
#[derive(Debug, Clone, Deserialize)]
pub struct ReportSpec {
    /// Report id, a hex string like `"0x01"`. Byte 0 of every report.
    #[serde(rename = "reportId")]
    pub report_id: String,
    /// Total report length in bytes.
    pub size: usize,
    pub fields: Vec<FieldSpec>,
    /// Whether the device streams this vendor report from power-on (Switch 2 Pro,
    /// Valve) rather than switching into it on a host handshake. When set, this is
    /// the profile's input report and its id is preferred during descriptor parse.
    #[serde(rename = "alwaysArmed", default)]
    pub always_armed: Option<bool>,
}

impl ReportSpec {
    /// The report-id byte, decoded from the `"0x01"` string.
    pub fn report_id_byte(&self) -> u8 {
        parse_hex_u8(&self.report_id).unwrap_or(0)
    }
}

/// One field of a report: what it carries, where, and how it is packed.
///
/// Every member is optional because a field uses only the parameters its type
/// needs — a `uint8-axis` has `byte`/`semantic`/`center`, a `bitfield` has
/// `bytes`/`bits`/`buttons`, and so on. The codec reads only what its op wants.
#[derive(Debug, Clone, Deserialize)]
pub struct FieldSpec {
    #[serde(rename = "type")]
    pub kind: String,
    pub semantic: Option<String>,
    /// Single byte offset.
    pub byte: Option<i64>,
    /// Byte range, `"lo-hi"` inclusive, for multi-byte fields.
    pub bytes: Option<String>,
    /// Bit range within a byte, `"lo-hi"` inclusive; defaults to the whole byte.
    pub bits: Option<String>,
    /// On-wire centre for `uint8-axis` (default 128).
    pub center: Option<i64>,
    /// Hat neutral value (default 8).
    #[serde(rename = "neutralValue")]
    pub neutral_value: Option<i64>,
    /// Constant / rolling-counter start.
    pub initial: Option<i64>,
    /// Rolling-counter stride (default 1).
    pub stride: Option<i64>,
    /// Button or flag names, one per bit position, for `button-mask` / `bitfield`.
    pub buttons: Option<Vec<String>>,
    /// CRC coverage, for `crc32-le`: a prefix and an inclusive byte range.
    pub scope: Option<CrcScope>,
}

/// What a `crc32-le` field covers: some literal prefix bytes, then a byte range
/// of the report itself. DualSense's Bluetooth report seeds the CRC with the
/// `0xA1 0x31` report header before the payload.
#[derive(Debug, Clone, Deserialize)]
pub struct CrcScope {
    pub prefix: Option<Vec<u8>>,
    pub from: i32,
    pub to: i32,
}

impl ReportSpec {
    /// Parse a spec from JSON. Used for tests and at controller creation.
    pub fn from_json(json: &str) -> Result<ReportSpec, serde_json::Error> {
        serde_json::from_str(json)
    }
}

/// Parse a `"0x01"` / `"1"` byte string.
pub fn parse_hex_u8(s: &str) -> Option<u8> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u8::from_str_radix(hex, 16).ok()
    } else {
        s.parse().ok()
    }
}

/// Parse a `"lo-hi"` inclusive range. Returns `(lo, hi)`.
pub fn parse_range(s: &str) -> Option<(i32, i32)> {
    let (lo, hi) = s.split_once('-')?;
    Some((lo.trim().parse().ok()?, hi.trim().parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_id_string_decodes_to_its_byte() {
        assert_eq!(parse_hex_u8("0x01"), Some(1));
        assert_eq!(parse_hex_u8("0x31"), Some(0x31));
        assert_eq!(parse_hex_u8("5"), Some(5));
    }

    #[test]
    fn a_range_string_splits_inclusively() {
        assert_eq!(parse_range("3-6"), Some((3, 6)));
        assert_eq!(parse_range("0-7"), Some((0, 7)));
    }

    #[test]
    fn a_minimal_spec_parses() {
        let spec = ReportSpec::from_json(
            r#"{"reportId":"0x01","size":10,
                "fields":[{"byte":1,"type":"uint8-axis","semantic":"leftStickX","center":128}]}"#,
        )
        .expect("parse");
        assert_eq!(spec.report_id_byte(), 1);
        assert_eq!(spec.size, 10);
        assert_eq!(spec.fields[0].kind, "uint8-axis");
        assert_eq!(spec.fields[0].semantic.as_deref(), Some("leftStickX"));
        assert_eq!(spec.fields[0].center, Some(128));
    }
}

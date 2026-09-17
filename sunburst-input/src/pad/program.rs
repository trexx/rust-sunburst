// SPDX-License-Identifier: GPL-2.0-or-later

//! The compiled form of a [`ReportSpec`] — a field list the codec walks.
//!
//! Ported from HIDMaestro's `VendorBlobProgram`. The spec's strings (a `type`,
//! a `semantic`, a `"lo-hi"` range) are resolved to numeric ops once here, at
//! controller creation, so the per-frame codec switches on an enum rather than
//! re-parsing strings. Byte-for-byte parity with HIDMaestro is the contract; it
//! is locked by the golden-hash test in [`super::golden`], which reproduces all
//! 63 of HIDMaestro's committed report hashes.

use super::spec::{CrcScope, FieldSpec, ReportSpec, parse_range};

/// D-pad octant. Values 1..8 (N..NW) map to a descriptor's 0..7; `None` maps to
/// the field's neutral. Mirrors HIDMaestro's `HMHat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Hat {
    #[default]
    None = 0,
    North = 1,
    NorthEast = 2,
    East = 3,
    SouthEast = 4,
    South = 5,
    SouthWest = 6,
    West = 7,
    NorthWest = 8,
}

/// The `HMButton` bit for a name, or a sentinel above the 32-bit mask for the
/// values that are not a plain button bit. `None`/`_`/unresolved → `0` (skip).
pub mod button {
    // Plain HMButton bits.
    pub const A: u64 = 1 << 0;
    pub const B: u64 = 1 << 1;
    pub const X: u64 = 1 << 2;
    pub const Y: u64 = 1 << 3;
    pub const LEFT_BUMPER: u64 = 1 << 4;
    pub const RIGHT_BUMPER: u64 = 1 << 5;
    pub const BACK: u64 = 1 << 6;
    pub const START: u64 = 1 << 7;
    pub const LEFT_STICK: u64 = 1 << 8;
    pub const RIGHT_STICK: u64 = 1 << 9;
    pub const GUIDE: u64 = 1 << 10;
    pub const TOUCHPAD: u64 = 1 << 11;
    pub const SHARE: u64 = 1 << 12;
    pub const RIGHT_PADDLE: u64 = 1 << 13;
    pub const LEFT_PADDLE: u64 = 1 << 14;
    pub const MISC1: u64 = 1 << 15;
    pub const RIGHT_PADDLE2: u64 = 1 << 16;
    pub const LEFT_PADDLE2: u64 = 1 << 17;

    // Sentinels above the mask space: values sourced from something other than
    // the button word.
    pub const LT_DIGITAL: u64 = 1 << 32;
    pub const RT_DIGITAL: u64 = 1 << 33;
    pub const DPAD_UP: u64 = 1 << 34;
    pub const DPAD_DOWN: u64 = 1 << 35;
    pub const DPAD_LEFT: u64 = 1 << 36;
    pub const DPAD_RIGHT: u64 = 1 << 37;
    pub const PAD0_TOUCH: u64 = 1 << 38;
    pub const PAD1_TOUCH: u64 = 1 << 39;

    /// A button name that resolves to no control and is not a recognised special
    /// name — a malformed profile. HIDMaestro throws on this rather than silently
    /// dropping a control (issue #58); the port names it at compile time.
    #[derive(Debug, PartialEq, Eq)]
    pub struct UnknownButton;

    /// Resolve a button name to its bit, case-insensitively, including the Sony
    /// aliases and the special sentinels. `None` for `_`/empty; `Err` for a name
    /// that resolves to nothing (HIDMaestro throws rather than silently drop a
    /// control — issue #58).
    pub fn resolve(name: &str) -> Result<Option<u64>, UnknownButton> {
        if name.is_empty() || name == "_" {
            return Ok(None);
        }
        let special = match name {
            "LT_DIGITAL" => Some(LT_DIGITAL),
            "RT_DIGITAL" => Some(RT_DIGITAL),
            "DPAD_UP" => Some(DPAD_UP),
            "DPAD_DOWN" => Some(DPAD_DOWN),
            "DPAD_LEFT" => Some(DPAD_LEFT),
            "DPAD_RIGHT" => Some(DPAD_RIGHT),
            "LEFTPAD_TOUCH" => Some(PAD0_TOUCH),
            "RIGHTPAD_TOUCH" => Some(PAD1_TOUCH),
            _ => None,
        };
        if let Some(bit) = special {
            return Ok(Some(bit));
        }
        let bit = match name.to_ascii_lowercase().as_str() {
            "a" | "cross" => A,
            "b" | "circle" => B,
            "x" | "square" => X,
            "y" | "triangle" => Y,
            "leftbumper" => LEFT_BUMPER,
            "rightbumper" => RIGHT_BUMPER,
            "back" => BACK,
            "start" => START,
            "leftstick" => LEFT_STICK,
            "rightstick" => RIGHT_STICK,
            "guide" => GUIDE,
            "touchpad" => TOUCHPAD,
            "share" => SHARE,
            "rightpaddle" => RIGHT_PADDLE,
            "leftpaddle" => LEFT_PADDLE,
            "misc1" => MISC1,
            "rightpaddle2" => RIGHT_PADDLE2,
            "leftpaddle2" => LEFT_PADDLE2,
            _ => return Err(UnknownButton),
        };
        Ok(Some(bit))
    }
}

/// A `bitfield` position's source flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagKind {
    None,
    Charging,
    Full,
    Mic,
    Headphones,
}

impl FlagKind {
    fn from_name(name: &str) -> FlagKind {
        match name {
            "batteryCharging" => FlagKind::Charging,
            "batteryFull" => FlagKind::Full,
            "micMuted" => FlagKind::Mic,
            "headphonesConnected" => FlagKind::Headphones,
            _ => FlagKind::None,
        }
    }
}

/// Wire-format family of a field. Numeric mirror of the JSON `type` strings.
///
/// `Unknown` preserves HIDMaestro's silent-no-op contract for a `type` string
/// the codec does not recognise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldOp {
    Unknown,
    U8Axis,
    U8Trigger,
    U8Rolling,
    U8Const,
    I16,
    U32,
    TouchpadFinger,
    Bitfield,
    Battery,
    HatOctant,
    ButtonMask,
    Rgb24,
    BytesPassthrough,
    BytesZero,
    Crc32,
    Stick12Pair,
    I16Axis,
    U16Trigger,
    U32Rolling,
    I16Pad,
    I16PadOrStick,
    U16Pressure,
}

impl FieldOp {
    fn from_type(kind: &str) -> FieldOp {
        match kind {
            "uint8-axis" => FieldOp::U8Axis,
            "uint8-trigger" => FieldOp::U8Trigger,
            "uint8-rolling" => FieldOp::U8Rolling,
            "uint8" => FieldOp::U8Const,
            "int16-le" => FieldOp::I16,
            "int16-axis" => FieldOp::I16Axis,
            "int16-pad" => FieldOp::I16Pad,
            "int16-pad-or-stick" => FieldOp::I16PadOrStick,
            "uint16-pressure" => FieldOp::U16Pressure,
            "uint16-trigger" => FieldOp::U16Trigger,
            "uint32-le" => FieldOp::U32,
            "uint32-rolling" => FieldOp::U32Rolling,
            "touchpad-finger" => FieldOp::TouchpadFinger,
            "bitfield" => FieldOp::Bitfield,
            "uint8-battery" => FieldOp::Battery,
            "hat-octant" => FieldOp::HatOctant,
            "button-mask" => FieldOp::ButtonMask,
            "rgb24" => FieldOp::Rgb24,
            "bytes-passthrough" => FieldOp::BytesPassthrough,
            "bytes-zero" => FieldOp::BytesZero,
            "crc32-le" => FieldOp::Crc32,
            "stick12-pair" => FieldOp::Stick12Pair,
            _ => FieldOp::Unknown,
        }
    }
}

/// Where an input field draws its value from. Numeric mirror of the `semantic`
/// strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SrcOp {
    None,
    LeftStickX,
    LeftStickY,
    RightStickX,
    RightStickY,
    LeftTrigger,
    RightTrigger,
    GyroPitch,
    GyroYaw,
    GyroRoll,
    AccelX,
    AccelY,
    AccelZ,
    SensorTimestamp,
    Finger0,
    Finger1,
    LeftStick,
    RightStick,
    LeftPadX,
    LeftPadY,
    RightPadX,
    RightPadY,
    LeftPadPressure,
    RightPadPressure,
}

impl SrcOp {
    fn from_semantic(semantic: Option<&str>) -> SrcOp {
        match semantic {
            Some("leftStickX") => SrcOp::LeftStickX,
            Some("leftStickY") => SrcOp::LeftStickY,
            Some("rightStickX") => SrcOp::RightStickX,
            Some("rightStickY") => SrcOp::RightStickY,
            Some("leftTrigger") => SrcOp::LeftTrigger,
            Some("rightTrigger") => SrcOp::RightTrigger,
            Some("gyroPitch") => SrcOp::GyroPitch,
            Some("gyroYaw") => SrcOp::GyroYaw,
            Some("gyroRoll") => SrcOp::GyroRoll,
            Some("accelX") => SrcOp::AccelX,
            Some("accelY") => SrcOp::AccelY,
            Some("accelZ") => SrcOp::AccelZ,
            Some("sensorTimestamp") => SrcOp::SensorTimestamp,
            Some("touchpadFinger1") => SrcOp::Finger1,
            Some("touchpadFinger0") => SrcOp::Finger0,
            Some("leftStick") => SrcOp::LeftStick,
            Some("rightStick") => SrcOp::RightStick,
            Some("leftPadX") => SrcOp::LeftPadX,
            Some("leftPadY") => SrcOp::LeftPadY,
            Some("rightPadX") => SrcOp::RightPadX,
            Some("rightPadY") => SrcOp::RightPadY,
            Some("leftPadPressure") => SrcOp::LeftPadPressure,
            Some("rightPadPressure") => SrcOp::RightPadPressure,
            _ => SrcOp::None,
        }
    }
}

/// One field, resolved to what the codec needs.
#[derive(Debug, Clone)]
pub struct CompiledField {
    pub op: FieldOp,
    pub source: SrcOp,
    /// The raw `semantic` string, kept for the output/decode directions which key
    /// a value dictionary by it (the input direction resolves it to [`source`]).
    pub semantic: Option<String>,
    /// Byte offset, or -1 when the field has no single byte.
    pub byte: i32,
    pub range_lo: i32,
    pub range_hi: i32,
    pub bit_lo: i32,
    pub bit_hi: i32,
    pub has_bits: bool,
    pub center: i32,
    pub neutral: i32,
    pub initial: u8,
    pub stride: i32,
    /// Rolling-counter key, precomputed.
    pub roll_key: String,
    /// Resolved button bits (per position, `0` = skip), for `button-mask`.
    pub button_bits: Option<Vec<u64>>,
    /// Resolved flag sources (per position), for `bitfield`.
    pub flag_kinds: Option<Vec<FlagKind>>,
    /// CRC destination start, or -1 to resolve from the buffer end at use.
    pub crc_dst: i32,
    /// CRC coverage, for `crc32-le`.
    pub scope: Option<CrcScope>,
}

/// A compiled report program.
#[derive(Debug, Clone)]
pub struct Program {
    pub report_id: u8,
    pub size: usize,
    pub fields: Vec<CompiledField>,
}

/// A malformed spec, named at compile time rather than crashing on the first
/// frame — HIDMaestro's own contract (audit of issue #34).
#[derive(Debug, PartialEq, Eq)]
pub struct CompileError(pub String);

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for CompileError {}

/// Resolve a spec into a program.
pub fn compile(spec: &ReportSpec) -> Result<Program, CompileError> {
    let mut fields = Vec::with_capacity(spec.fields.len());
    for (i, f) in spec.fields.iter().enumerate() {
        fields.push(compile_field(i, f)?);
    }
    Ok(Program {
        report_id: spec.report_id_byte(),
        size: spec.size,
        fields,
    })
}

fn compile_field(index: usize, f: &FieldSpec) -> Result<CompiledField, CompileError> {
    let op = FieldOp::from_type(&f.kind);
    let mut source = SrcOp::from_semantic(f.semantic.as_deref());

    // touchpad-finger defaults to finger0 for any semantic other than finger1,
    // matching HIDMaestro's else-branch.
    if op == FieldOp::TouchpadFinger && source != SrcOp::Finger1 {
        source = SrcOp::Finger0;
    }

    if let Some(declared) = f.byte
        && declared < 0
    {
        return Err(CompileError(format!(
            "extendedReport field {index} (type '{}', semantic {:?}) declares negative byte offset {declared}",
            f.kind, f.semantic
        )));
    }

    let byte = f.byte.map(|b| b as i32).unwrap_or(-1);
    let (has_bytes, range_lo, range_hi) = match f.bytes.as_deref().and_then(parse_range) {
        Some((lo, hi)) => (true, lo, hi),
        None => (false, -1, -1),
    };
    let (has_bits, bit_lo, bit_hi) = match f.bits.as_deref().and_then(parse_range) {
        Some((lo, hi)) => (true, lo, hi),
        None => (false, 0, 7),
    };

    let roll_key = f
        .semantic
        .clone()
        .unwrap_or_else(|| format!("_b{}", if byte >= 0 { byte } else { 0 }));

    // CRC dest resolves from a bytes-range start, else the byte, else -1 (the
    // buffer end at use time — a short host write shortens the buffer).
    let crc_dst = if has_bytes { range_lo } else { byte };

    let mut button_bits = None;
    let mut flag_kinds = None;
    if let Some(names) = &f.buttons {
        if op == FieldOp::ButtonMask {
            let mut bits = Vec::with_capacity(names.len());
            for (j, name) in names.iter().enumerate() {
                match button::resolve(name) {
                    Ok(bit) => bits.push(bit.unwrap_or(0)),
                    Err(button::UnknownButton) => {
                        return Err(CompileError(format!(
                            "button name '{name}' at index {j} of the button mask at byte {byte} \
resolves to no button and is not a recognised special name"
                        )));
                    }
                }
            }
            button_bits = Some(bits);
        } else if op == FieldOp::Bitfield {
            flag_kinds = Some(names.iter().map(|n| FlagKind::from_name(n)).collect());
        }
    }

    Ok(CompiledField {
        op,
        source,
        semantic: f.semantic.clone(),
        byte,
        range_lo,
        range_hi,
        bit_lo,
        bit_hi,
        has_bits,
        center: f.center.map(|c| c as i32).unwrap_or(128),
        neutral: f.neutral_value.map(|n| n as i32).unwrap_or(8),
        initial: f.initial.unwrap_or(0) as u8,
        stride: f.stride.map(|s| s as i32).unwrap_or(1),
        roll_key,
        button_bits,
        flag_kinds,
        crc_dst,
        scope: f.scope.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(kind: &str, semantic: Option<&str>, byte: i64) -> FieldSpec {
        FieldSpec {
            kind: kind.into(),
            semantic: semantic.map(str::to_string),
            byte: Some(byte),
            bytes: None,
            bits: None,
            center: None,
            neutral_value: None,
            initial: None,
            stride: None,
            buttons: None,
            scope: None,
        }
    }

    #[test]
    fn type_and_semantic_strings_resolve_to_ops() {
        let c = compile_field(0, &field("uint8-axis", Some("leftStickX"), 1)).expect("compile");
        assert_eq!(c.op, FieldOp::U8Axis);
        assert_eq!(c.source, SrcOp::LeftStickX);
        assert_eq!(c.byte, 1);
        assert_eq!(c.center, 128);
    }

    #[test]
    fn an_unknown_type_is_the_silent_no_op() {
        let c = compile_field(0, &field("something-new", None, 3)).expect("compile");
        assert_eq!(c.op, FieldOp::Unknown);
    }

    #[test]
    fn a_negative_byte_offset_is_named_at_compile_time() {
        let err = compile_field(2, &field("uint8-axis", Some("leftStickX"), -1)).unwrap_err();
        assert!(err.0.contains("field 2"), "{}", err.0);
        assert!(err.0.contains("negative byte offset"));
    }

    #[test]
    fn touchpad_finger_defaults_to_finger0() {
        let c = compile_field(0, &field("touchpad-finger", Some("somethingElse"), 5)).expect("c");
        assert_eq!(c.source, SrcOp::Finger0);
        let c1 =
            compile_field(0, &field("touchpad-finger", Some("touchpadFinger1"), 5)).expect("c");
        assert_eq!(c1.source, SrcOp::Finger1);
    }
}

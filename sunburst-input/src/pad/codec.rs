// SPDX-License-Identifier: GPL-2.0-or-later

//! The generic report codec — the single walker that serves every profile.
//!
//! Ported from HIDMaestro's `VendorBlobCodec`. Byte-for-byte parity is the
//! contract: the same input state through the same profile must produce the same
//! wire bytes HIDMaestro's driver expects, or a game rejects the pad. That
//! contract is proven — [`super::golden`] reproduces all 63 of HIDMaestro's own
//! committed report hashes (input, output and decode) across its nine Sony
//! profiles. The Valve state-packet ops carry no committed golden (HIDMaestro's
//! table is Sony only); they are transcribed faithfully and unit-tested by hand.
//!
//! # Rounding
//!
//! C#'s `Math.Round(double)` is **round-half-to-even** (banker's rounding), not
//! round-half-away-from-zero as Rust's `f32::round` is. A stick at exactly 0.5
//! must land on 128, not 127 or 129, so [`round_half_even`] reproduces it. The
//! scale multiply is done in `f32` first, matching C#'s `float * int` before the
//! promotion to `double` for the round.

use std::collections::{BTreeMap, HashMap};

use super::program::{CompiledField, FieldOp, FlagKind, Hat, Program, SrcOp, button};
use super::spec::CrcScope;

/// The input state one frame carries. Sticks and triggers are `[0, 1]` floats
/// (0.5 = centre), the IMU is raw `i16`, matching what HIDMaestro's encoder
/// takes.
#[derive(Debug, Clone, Default)]
pub struct InputState {
    pub left_stick_x: f32,
    pub left_stick_y: f32,
    pub right_stick_x: f32,
    pub right_stick_y: f32,
    pub left_trigger: f32,
    pub right_trigger: f32,
    pub buttons: u32,
    pub hat: Hat,
    pub gyro_pitch: i16,
    pub gyro_yaw: i16,
    pub gyro_roll: i16,
    pub accel_x: i16,
    pub accel_y: i16,
    pub accel_z: i16,
    pub sensor_timestamp: u32,
    pub finger0_active: bool,
    pub finger0_x: u16,
    pub finger0_y: u16,
    pub finger0_id: u8,
    pub finger1_active: bool,
    pub finger1_x: u16,
    pub finger1_y: u16,
    pub finger1_id: u8,
    pub battery_level: u8,
    pub battery_charging: bool,
    pub battery_full: bool,
    pub mic_muted: bool,
    pub headphones_connected: bool,
}

/// CRC-32/ISO-HDLC table (reflected poly 0xEDB88320), built at compile time.
const CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            if crc & 1 != 0 {
                crc = 0xEDB8_8320 ^ (crc >> 1);
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

fn compute_crc32(scope: &CrcScope, buffer: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    if let Some(prefix) = &scope.prefix {
        for &b in prefix {
            crc = CRC32_TABLE[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
        }
    }
    if scope.from >= 0 && !buffer.is_empty() {
        let from = scope.from as usize;
        let to = (scope.to.max(0) as usize).min(buffer.len() - 1);
        for &b in buffer.iter().take(to + 1).skip(from) {
            crc = CRC32_TABLE[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
        }
    }
    crc ^ 0xFFFF_FFFF
}

/// State that persists across frames: the rolling counters, keyed as the
/// compiler precomputed. `rolling32` is the wider counter Valve's 32-bit packet
/// number needs, kept distinct from the 8-bit counters exactly as HIDMaestro's
/// `RollingCounters` / `RollingCounters32` are.
#[derive(Debug, Default)]
pub struct EncoderState {
    rolling: HashMap<String, u8>,
    rolling32: HashMap<String, u32>,
}

/// Round half to even, as C#'s `Math.Round(double)` does.
fn round_half_even(x: f64) -> f64 {
    let frac = x - x.trunc();
    if frac.abs() == 0.5 {
        let lower = x.floor();
        if (lower as i64).rem_euclid(2) == 0 {
            lower
        } else {
            lower + 1.0
        }
    } else {
        x.round()
    }
}

fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

/// `clamp(v,0,1) * scale`, rounded half-even, clamped to `[0, scale]` — the
/// unsigned-axis/trigger path.
fn scale_unit(v: f32, scale: i32) -> i32 {
    let raw = round_half_even((clamp01(v) * scale as f32) as f64) as i32;
    raw.clamp(0, scale)
}

/// Encode one input frame into `buffer` (at least `program.size` bytes).
///
/// Faithful to `VendorBlobCodec.EncodeInput`. Every input-direction op is
/// handled; output-only types (rgb24, bytes-passthrough) and unrecognised types
/// are the deliberate no-ops HIDMaestro's own contract specifies.
pub fn encode_input(
    program: &Program,
    state: &InputState,
    buffer: &mut [u8],
    enc: &mut EncoderState,
) {
    let size = program.size;
    buffer[..size].fill(0);
    buffer[0] = program.report_id;

    for f in &program.fields {
        match f.op {
            FieldOp::U8Axis => u8_axis(f, state, buffer),
            FieldOp::U8Trigger => u8_trigger(f, state, buffer),
            FieldOp::Stick12Pair => stick12_pair(f, state, buffer),
            FieldOp::U8Rolling => u8_rolling(f, buffer, enc),
            FieldOp::U8Const => {
                if f.byte >= 0 {
                    buffer[f.byte as usize] = f.initial;
                }
            }
            FieldOp::I16 => i16_le(f, state, buffer),
            FieldOp::I16Axis => i16_axis(f, state, buffer),
            FieldOp::U32 => u32_le(f, state, buffer),
            FieldOp::TouchpadFinger => touchpad_finger(f, state, buffer),
            FieldOp::Bitfield => bitfield(f, state, buffer),
            FieldOp::Battery => battery(f, state, buffer),
            FieldOp::HatOctant => hat_octant(f, state, buffer),
            FieldOp::ButtonMask => button_mask(f, state, buffer),
            FieldOp::Crc32 => crc32(f, buffer),
            // Valve's 16/32-bit state-packet ops (Steam Controller / Deck). No
            // shipped golden covers them — HIDMaestro's table is Sony only — so
            // these are transcribed from `EncodeInput` and unit-tested by hand.
            FieldOp::U16Trigger => u16_trigger(f, state, buffer),
            FieldOp::U16Pressure => u16_pressure(f, state, buffer),
            FieldOp::U32Rolling => u32_rolling(f, buffer, enc),
            FieldOp::I16Pad | FieldOp::I16PadOrStick => i16_pad(f, state, buffer),
            // Rgb24 / BytesPassthrough / BytesZero carry no input source
            // (output-only), and Unknown is HIDMaestro's silent-no-op contract:
            // the buffer is already zeroed.
            FieldOp::Rgb24 | FieldOp::BytesPassthrough | FieldOp::BytesZero | FieldOp::Unknown => {}
        }
    }
}

fn u8_axis(f: &CompiledField, state: &InputState, buffer: &mut [u8]) {
    if f.byte < 0 {
        return;
    }
    let v = match f.source {
        SrcOp::LeftStickX => state.left_stick_x,
        SrcOp::LeftStickY => state.left_stick_y,
        SrcOp::RightStickX => state.right_stick_x,
        SrcOp::RightStickY => state.right_stick_y,
        _ => 0.5,
    };
    let raw = if f.center != 128 {
        f.center + round_half_even(((clamp01(v) - 0.5) * 254.0) as f64) as i32
    } else {
        scale_unit(v, 255)
    };
    buffer[f.byte as usize] = raw.clamp(0, 255) as u8;
}

fn u8_trigger(f: &CompiledField, state: &InputState, buffer: &mut [u8]) {
    if f.byte < 0 {
        return;
    }
    let v = match f.source {
        SrcOp::LeftTrigger => state.left_trigger,
        SrcOp::RightTrigger => state.right_trigger,
        _ => 0.0,
    };
    buffer[f.byte as usize] = scale_unit(v, 255) as u8;
}

fn stick12_pair(f: &CompiledField, state: &InputState, buffer: &mut [u8]) {
    let b = f.byte;
    if b < 0 || (b + 2) as usize >= buffer.len() {
        return;
    }
    let (vx, vy) = if f.source == SrcOp::RightStick {
        (state.right_stick_x, state.right_stick_y)
    } else {
        (state.left_stick_x, state.left_stick_y)
    };
    let x = scale_unit(vx, 4095);
    let y = scale_unit(vy, 4095);
    let b = b as usize;
    buffer[b] = (x & 0xFF) as u8;
    buffer[b + 1] = (((x >> 8) & 0x0F) | ((y & 0x0F) << 4)) as u8;
    buffer[b + 2] = ((y >> 4) & 0xFF) as u8;
}

fn u8_rolling(f: &CompiledField, buffer: &mut [u8], enc: &mut EncoderState) {
    if f.byte < 0 {
        return;
    }
    let counter = *enc.rolling.get(&f.roll_key).unwrap_or(&f.initial);
    buffer[f.byte as usize] = counter;
    enc.rolling
        .insert(f.roll_key.clone(), counter.wrapping_add(f.stride as u8));
}

fn i16_le(f: &CompiledField, state: &InputState, buffer: &mut [u8]) {
    let b = f.byte;
    if b < 0 || (b + 1) as usize >= buffer.len() {
        return;
    }
    let v = match f.source {
        SrcOp::GyroPitch => state.gyro_pitch,
        SrcOp::GyroYaw => state.gyro_yaw,
        SrcOp::GyroRoll => state.gyro_roll,
        SrcOp::AccelX => state.accel_x,
        SrcOp::AccelY => state.accel_y,
        SrcOp::AccelZ => state.accel_z,
        _ => 0,
    };
    let b = b as usize;
    buffer[b] = (v & 0xFF) as u8;
    buffer[b + 1] = ((v >> 8) & 0xFF) as u8;
}

fn i16_axis(f: &CompiledField, state: &InputState, buffer: &mut [u8]) {
    let b = f.byte;
    if b < 0 || (b + 1) as usize >= buffer.len() {
        return;
    }
    let sv = match f.source {
        SrcOp::LeftStickX => state.left_stick_x,
        SrcOp::LeftStickY => state.left_stick_y,
        SrcOp::RightStickX => state.right_stick_x,
        SrcOp::RightStickY => state.right_stick_y,
        _ => 0.5,
    };
    let y_axis = matches!(f.source, SrcOp::LeftStickY | SrcOp::RightStickY);
    let mut centred = clamp01(sv) - 0.5;
    if y_axis {
        centred = -centred;
    }
    let sraw =
        (round_half_even((centred * 2.0 * 32767.0) as f64) as i32).clamp(-32767, 32767) as i16;
    let b = b as usize;
    buffer[b] = (sraw & 0xFF) as u8;
    buffer[b + 1] = ((sraw >> 8) & 0xFF) as u8;
}

fn u32_le(f: &CompiledField, state: &InputState, buffer: &mut [u8]) {
    let b = f.byte;
    if b < 0 || (b + 3) as usize >= buffer.len() {
        return;
    }
    let v = if f.source == SrcOp::SensorTimestamp {
        state.sensor_timestamp
    } else {
        0
    };
    let b = b as usize;
    buffer[b] = (v & 0xFF) as u8;
    buffer[b + 1] = ((v >> 8) & 0xFF) as u8;
    buffer[b + 2] = ((v >> 16) & 0xFF) as u8;
    buffer[b + 3] = ((v >> 24) & 0xFF) as u8;
}

fn u16_trigger(f: &CompiledField, state: &InputState, buffer: &mut [u8]) {
    let b = f.byte;
    if b < 0 || (b + 1) as usize >= buffer.len() {
        return;
    }
    let tv = match f.source {
        SrcOp::LeftTrigger => state.left_trigger,
        SrcOp::RightTrigger => state.right_trigger,
        _ => 0.0,
    };
    // 0..1 onto 0..32767 (SDL widens as raw*2-32768), so a full pull is 32767.
    let traw = round_half_even((clamp01(tv) * 32767.0f32) as f64) as i32;
    let b = b as usize;
    buffer[b] = (traw & 0xFF) as u8;
    buffer[b + 1] = ((traw >> 8) & 0xFF) as u8;
}

fn u16_pressure(f: &CompiledField, state: &InputState, buffer: &mut [u8]) {
    let b = f.byte;
    if b < 0 || (b + 1) as usize >= buffer.len() {
        return;
    }
    // A contact reports full scale, no contact zero — the shape a capacitive pad
    // without a force sensor reports (HMGamepadState carries no analog pressure).
    let pdown = if f.source == SrcOp::RightPadPressure {
        state.finger1_active
    } else {
        state.finger0_active
    };
    let pv: u16 = if pdown { 32767 } else { 0 };
    let b = b as usize;
    buffer[b] = (pv & 0xFF) as u8;
    buffer[b + 1] = ((pv >> 8) & 0xFF) as u8;
}

fn u32_rolling(f: &CompiledField, buffer: &mut [u8], enc: &mut EncoderState) {
    let b = f.byte;
    if b < 0 || (b + 3) as usize >= buffer.len() {
        return;
    }
    // Valve's unPacketNum: a consumer that sees the same number twice may skip
    // the frame, so it must advance on every encode.
    let c32 = *enc
        .rolling32
        .get(&f.roll_key)
        .unwrap_or(&(f.initial as u32));
    let b = b as usize;
    buffer[b] = (c32 & 0xFF) as u8;
    buffer[b + 1] = ((c32 >> 8) & 0xFF) as u8;
    buffer[b + 2] = ((c32 >> 16) & 0xFF) as u8;
    buffer[b + 3] = ((c32 >> 24) & 0xFF) as u8;
    let step = f.stride.max(1) as u32;
    enc.rolling32
        .insert(f.roll_key.clone(), c32.wrapping_add(step));
}

/// Valve trackpad coordinate (`int16-pad`), and the 2015 controller's shared
/// pad/stick pair (`int16-pad-or-stick`). A finger-down contact maps Sony's
/// native finger range (0..1919 × 0..1079) onto full-scale signed with Y up;
/// `int16-pad-or-stick` falls back to the joystick when the finger is up, which
/// is the hardware's own behaviour.
fn i16_pad(f: &CompiledField, state: &InputState, buffer: &mut [u8]) {
    let b = f.byte;
    if b < 0 || (b + 1) as usize >= buffer.len() {
        return;
    }
    let is_left = matches!(f.source, SrcOp::LeftPadX | SrcOp::LeftPadY);
    let is_y = matches!(f.source, SrcOp::LeftPadY | SrcOp::RightPadY);
    let down = if is_left {
        state.finger0_active
    } else {
        state.finger1_active
    };
    let praw: i16 = if down {
        let raw = if is_left {
            if is_y {
                state.finger0_y
            } else {
                state.finger0_x
            }
        } else if is_y {
            state.finger1_y
        } else {
            state.finger1_x
        };
        let span = if is_y { 1079.0f32 } else { 1919.0f32 };
        let mut unit = clamp01(raw as f32 / span) - 0.5;
        if is_y {
            unit = -unit;
        }
        (round_half_even((unit * 2.0 * 32767.0) as f64) as i32).clamp(-32767, 32767) as i16
    } else if f.op == FieldOp::I16PadOrStick {
        let sv = if is_left {
            if is_y {
                state.left_stick_y
            } else {
                state.left_stick_x
            }
        } else if is_y {
            state.right_stick_y
        } else {
            state.right_stick_x
        };
        let mut centred = clamp01(sv) - 0.5;
        if is_y {
            centred = -centred;
        }
        (round_half_even((centred * 2.0 * 32767.0) as f64) as i32).clamp(-32767, 32767) as i16
    } else {
        0
    };
    let b = b as usize;
    buffer[b] = (praw & 0xFF) as u8;
    buffer[b + 1] = ((praw >> 8) & 0xFF) as u8;
}

fn touchpad_finger(f: &CompiledField, state: &InputState, buffer: &mut [u8]) {
    let b = f.byte;
    if b < 0 || (b + 3) as usize >= buffer.len() {
        return;
    }
    let (active, x, y, id) = if f.source == SrcOp::Finger1 {
        (
            state.finger1_active,
            state.finger1_x,
            state.finger1_y,
            state.finger1_id,
        )
    } else {
        (
            state.finger0_active,
            state.finger0_x,
            state.finger0_y,
            state.finger0_id,
        )
    };
    let b = b as usize;
    // Bit 7 of byte 0 is "lifted" — set when NOT touching.
    buffer[b] = (id & 0x7F) | if active { 0x00 } else { 0x80 };
    buffer[b + 1] = (x & 0xFF) as u8;
    buffer[b + 2] = (((x >> 8) & 0x0F) | ((y & 0x0F) << 4)) as u8;
    buffer[b + 3] = ((y >> 4) & 0xFF) as u8;
}

fn bitfield(f: &CompiledField, state: &InputState, buffer: &mut [u8]) {
    let b = f.byte;
    if b < 0 || b as usize >= buffer.len() {
        return;
    }
    let Some(kinds) = &f.flag_kinds else { return };
    let mut packed = 0u8;
    for (i, kind) in kinds.iter().enumerate() {
        if f.bit_lo + i as i32 > f.bit_hi {
            break;
        }
        let bit = match kind {
            FlagKind::Charging => state.battery_charging,
            FlagKind::Full => state.battery_full,
            FlagKind::Mic => state.mic_muted,
            FlagKind::Headphones => state.headphones_connected,
            FlagKind::None => false,
        };
        if bit {
            packed |= 1u8 << (f.bit_lo + i as i32);
        }
    }
    let width = f.bit_hi - f.bit_lo + 1;
    let preserve = !((((1u32 << width) - 1) << f.bit_lo) as u8);
    buffer[b as usize] = (buffer[b as usize] & preserve) | packed;
}

fn battery(f: &CompiledField, state: &InputState, buffer: &mut [u8]) {
    let b = f.byte;
    if b < 0 || b as usize >= buffer.len() {
        return;
    }
    let width = f.bit_hi - f.bit_lo + 1;
    let mask = ((((1u32 << width) - 1) << f.bit_lo) & 0xFF) as u8;
    let v = state.battery_level & (((1u32 << width) - 1) as u8);
    buffer[b as usize] = (buffer[b as usize] & !mask) | ((v << f.bit_lo) & mask);
}

fn hat_octant(f: &CompiledField, state: &InputState, buffer: &mut [u8]) {
    let b = f.byte;
    if b < 0 {
        return;
    }
    let nibble = if state.hat == Hat::None {
        f.neutral
    } else {
        (state.hat as i32 - 1) & 0x0F
    };
    let b = b as usize;
    if f.has_bits {
        let width = f.bit_hi - f.bit_lo + 1;
        let mask = ((((1u32 << width) - 1) << f.bit_lo) & 0xFF) as u8;
        buffer[b] = (buffer[b] & !mask) | (((nibble << f.bit_lo) as u8) & mask);
    } else {
        buffer[b] = (nibble & 0xFF) as u8;
    }
}

fn button_mask(f: &CompiledField, state: &InputState, buffer: &mut [u8]) {
    let b = f.byte;
    let Some(bits_list) = &f.button_bits else {
        return;
    };
    if b < 0 {
        return;
    }
    let h = state.hat;
    let mut packed = 0u64;
    for (i, &bits) in bits_list.iter().enumerate() {
        if f.bit_lo + i as i32 > f.bit_hi {
            break;
        }
        if bits == 0 {
            continue;
        }
        let on = match bits {
            button::LT_DIGITAL => state.left_trigger > 0.0,
            button::RT_DIGITAL => state.right_trigger > 0.0,
            button::DPAD_UP => matches!(h, Hat::North | Hat::NorthEast | Hat::NorthWest),
            button::DPAD_DOWN => matches!(h, Hat::South | Hat::SouthEast | Hat::SouthWest),
            button::DPAD_LEFT => matches!(h, Hat::West | Hat::NorthWest | Hat::SouthWest),
            button::DPAD_RIGHT => matches!(h, Hat::East | Hat::NorthEast | Hat::SouthEast),
            button::PAD0_TOUCH => state.finger0_active,
            button::PAD1_TOUCH => state.finger1_active,
            _ => (state.buttons & bits as u32) != 0,
        };
        if on {
            packed |= 1u64 << (f.bit_lo + i as i32);
        }
    }
    // OR into each byte the range spans, preserving bits outside it.
    let n_bytes = f.bit_hi / 8 + 1;
    for by in 0..n_bytes {
        let idx = b + by;
        if idx < 0 || idx as usize >= buffer.len() {
            break;
        }
        let lo = by * 8;
        let rlo = lo.max(f.bit_lo);
        let rhi = (lo + 7).min(f.bit_hi);
        if rlo > rhi {
            continue;
        }
        let span = ((((1u32 << (rhi - rlo + 1)) - 1) << (rlo - lo)) & 0xFF) as u8;
        let val = ((packed >> lo) & 0xFF) as u8;
        buffer[idx as usize] = (buffer[idx as usize] & !span) | (val & span);
    }
}

fn crc32(f: &CompiledField, buffer: &mut [u8]) {
    let Some(scope) = &f.scope else { return };
    let crc = compute_crc32(scope, buffer);
    let dst = if f.crc_dst >= 0 {
        f.crc_dst as usize
    } else {
        buffer.len().saturating_sub(4)
    };
    if dst + 3 < buffer.len() {
        buffer[dst] = (crc & 0xFF) as u8;
        buffer[dst + 1] = ((crc >> 8) & 0xFF) as u8;
        buffer[dst + 2] = ((crc >> 16) & 0xFF) as u8;
        buffer[dst + 3] = ((crc >> 24) & 0xFF) as u8;
    }
}

// ── Output encoder: semantic-keyed values → bytes ─────────────────────────────

/// One value a consumer supplies for an output-report field, keyed by semantic.
/// Mirrors the boxed `object` cases HIDMaestro's `EncodeOutput` distinguishes: a
/// plain byte, a float (an axis/trigger to scale), a raw blob (rgb24 / bytes
/// passthrough), or a packed u32 (rgb24's alternative form).
#[derive(Debug, Clone, PartialEq)]
pub enum OutValue {
    Byte(u8),
    Float(f32),
    Bytes(Vec<u8>),
    U32(u32),
}

/// `ToByte`, faithfully: only the integer-family boxed types yield their low
/// byte; a float or a blob yields 0 (HIDMaestro's `_ => 0`).
fn to_byte(v: &OutValue) -> u8 {
    match v {
        OutValue::Byte(b) => *b,
        OutValue::U32(u) => (*u & 0xFF) as u8,
        _ => 0,
    }
}

/// Encode an output report from a semantic-keyed value map into `buffer`.
///
/// Faithful to `VendorBlobCodec.EncodeOutput`: the buffer is zeroed, the report
/// id written at 0, then fields walk in spec order — each drawing its value from
/// `fields` (an unmapped field stays zero), `uint8-rolling` auto-advancing its
/// counter, and `crc32-le` sealing the result last. Fields that overlap (Sony's
/// whole-report `effectPayload` over the individual effect bytes) resolve by
/// order, exactly as HIDMaestro's do.
pub fn encode_output(
    program: &Program,
    fields: &HashMap<String, OutValue>,
    buffer: &mut [u8],
    enc: &mut EncoderState,
) {
    let size = program.size;
    buffer[..size].fill(0);
    buffer[0] = program.report_id;

    for f in &program.fields {
        match f.op {
            FieldOp::U8Const => out_u8_const(f, fields, buffer),
            FieldOp::U8Rolling => out_u8_rolling(f, fields, buffer, enc),
            FieldOp::U8Axis => out_u8_axis(f, fields, buffer),
            FieldOp::U8Trigger => out_u8_trigger(f, fields, buffer),
            FieldOp::Rgb24 => out_rgb24(f, fields, buffer),
            FieldOp::BytesPassthrough => out_bytes(f, fields, buffer),
            FieldOp::Crc32 => crc32(f, buffer),
            // BytesZero and every input-only / unknown type: leave zeroed.
            _ => {}
        }
    }
}

fn out_u8_const(f: &CompiledField, fields: &HashMap<String, OutValue>, buffer: &mut [u8]) {
    if f.byte < 0 {
        return;
    }
    let b = f.byte as usize;
    if b >= buffer.len() {
        return;
    }
    if let Some(val) = f.semantic.as_deref().and_then(|s| fields.get(s)) {
        buffer[b] = to_byte(val);
    } else {
        // A constant the spec wants even when the consumer omits it (Sony BT's
        // byte-2 framing flag 0x10). `initial` is 0 when undeclared, which equals
        // the zeroed default, so writing it unconditionally reproduces C#'s
        // "else if Initial.HasValue".
        buffer[b] = f.initial;
    }
}

fn out_u8_rolling(
    f: &CompiledField,
    fields: &HashMap<String, OutValue>,
    buffer: &mut [u8],
    enc: &mut EncoderState,
) {
    if f.byte < 0 {
        return;
    }
    let b = f.byte as usize;
    if b >= buffer.len() {
        return;
    }
    // An explicit consumer value overrides the auto-advance.
    if let Some(val) = f.semantic.as_deref().and_then(|s| fields.get(s)) {
        buffer[b] = to_byte(val);
        return;
    }
    // Auto-advance by stride (Sony BT btTag; real firmware drops the packet
    // otherwise). The output key is `semantic ?? _o{B}` — the `_o` prefix keeps
    // a semantic-less output counter distinct from the input side's `_b{B}`.
    let key = f
        .semantic
        .clone()
        .unwrap_or_else(|| format!("_o{}", f.byte));
    let counter = *enc.rolling.get(&key).unwrap_or(&f.initial);
    buffer[b] = counter;
    enc.rolling
        .insert(key, counter.wrapping_add(f.stride as u8));
}

fn out_u8_axis(f: &CompiledField, fields: &HashMap<String, OutValue>, buffer: &mut [u8]) {
    if f.byte < 0 {
        return;
    }
    let Some(sem) = f.semantic.as_deref() else {
        return;
    };
    let b = f.byte as usize;
    if b >= buffer.len() {
        return;
    }
    if let Some(val) = fields.get(sem) {
        buffer[b] = match val {
            OutValue::Float(fv) => {
                (f.center + round_half_even((*fv * 127.0f32) as f64) as i32).clamp(0, 255) as u8
            }
            other => to_byte(other),
        };
    } else {
        buffer[b] = f.center as u8;
    }
}

fn out_u8_trigger(f: &CompiledField, fields: &HashMap<String, OutValue>, buffer: &mut [u8]) {
    if f.byte < 0 {
        return;
    }
    let Some(sem) = f.semantic.as_deref() else {
        return;
    };
    let b = f.byte as usize;
    if b >= buffer.len() {
        return;
    }
    if let Some(val) = fields.get(sem) {
        buffer[b] = match val {
            OutValue::Float(fv) => {
                (round_half_even((*fv * 255.0f32) as f64) as i32).clamp(0, 255) as u8
            }
            other => to_byte(other),
        };
    }
}

fn out_rgb24(f: &CompiledField, fields: &HashMap<String, OutValue>, buffer: &mut [u8]) {
    if f.range_lo < 0 {
        return;
    }
    let Some(sem) = f.semantic.as_deref() else {
        return;
    };
    let lo = f.range_lo as usize;
    if lo + 2 >= buffer.len() {
        return;
    }
    match fields.get(sem) {
        Some(OutValue::Bytes(arr)) if arr.len() >= 3 => {
            buffer[lo] = arr[0];
            buffer[lo + 1] = arr[1];
            buffer[lo + 2] = arr[2];
        }
        Some(OutValue::U32(packed)) => {
            buffer[lo] = ((*packed >> 16) & 0xFF) as u8;
            buffer[lo + 1] = ((*packed >> 8) & 0xFF) as u8;
            buffer[lo + 2] = (*packed & 0xFF) as u8;
        }
        _ => {}
    }
}

fn out_bytes(f: &CompiledField, fields: &HashMap<String, OutValue>, buffer: &mut [u8]) {
    if f.range_lo < 0 {
        return;
    }
    let Some(sem) = f.semantic.as_deref() else {
        return;
    };
    let lo = f.range_lo as usize;
    if let Some(OutValue::Bytes(arr)) = fields.get(sem) {
        let span = (f.range_hi - f.range_lo + 1).max(0) as usize;
        let n = arr.len().min(span).min(buffer.len().saturating_sub(lo));
        buffer[lo..lo + n].copy_from_slice(&arr[..n]);
    }
}

// ── Decoder: bytes → semantic-keyed values ────────────────────────────────────

/// A value decoded from an output report, keyed by the field's semantic.
///
/// `Buttons` and the float axis/trigger forms exist for completeness with
/// HIDMaestro's `Decode`, but no shipped output report carries a stick, trigger,
/// hat or button (those are input semantics), so decode is exercised only through
/// `Byte`/`Bytes` in practice.
#[derive(Debug, Clone, PartialEq)]
pub enum DecodedValue {
    Byte(u8),
    Axis(f32),
    Trigger(f32),
    Hat(u8),
    Bytes(Vec<u8>),
}

/// Decode an output report into a semantic-keyed map, plus whether its CRC (if
/// the spec declares one) verified.
///
/// Faithful to `VendorBlobCodec.Decode`. The map is a `BTreeMap`, so it iterates
/// in key order — which for these ASCII semantics is the same ordinal order the
/// golden probe's canonical dump uses. `button-mask` decode is intentionally not
/// ported: it needs the raw button-name list, and no output report declares one.
pub fn decode(program: &Program, buffer: &[u8]) -> (BTreeMap<String, DecodedValue>, bool) {
    let mut result = BTreeMap::new();
    let mut crc_valid = true;

    for f in &program.fields {
        match f.op {
            FieldOp::U8Const | FieldOp::U8Rolling => {
                if f.byte < 0 {
                    continue;
                }
                let Some(sem) = f.semantic.as_deref() else {
                    continue;
                };
                let b = f.byte as usize;
                if b >= buffer.len() {
                    continue;
                }
                result.insert(sem.to_string(), DecodedValue::Byte(buffer[b]));
            }
            FieldOp::U8Axis => {
                if f.byte < 0 {
                    continue;
                }
                let Some(sem) = f.semantic.as_deref() else {
                    continue;
                };
                let b = f.byte as usize;
                if b >= buffer.len() {
                    continue;
                }
                let v = ((buffer[b] as i32 - f.center) as f64 / 127.0) as f32;
                result.insert(sem.to_string(), DecodedValue::Axis(v));
            }
            FieldOp::U8Trigger => {
                if f.byte < 0 {
                    continue;
                }
                let Some(sem) = f.semantic.as_deref() else {
                    continue;
                };
                let b = f.byte as usize;
                if b >= buffer.len() {
                    continue;
                }
                let v = (buffer[b] as f64 / 255.0) as f32;
                result.insert(sem.to_string(), DecodedValue::Trigger(v));
            }
            FieldOp::HatOctant => {
                if f.byte < 0 {
                    continue;
                }
                let Some(sem) = f.semantic.as_deref() else {
                    continue;
                };
                let b = f.byte as usize;
                if b >= buffer.len() {
                    continue;
                }
                let raw = if f.has_bits {
                    let width = f.bit_hi - f.bit_lo + 1;
                    let mask = (1i32 << width) - 1;
                    (buffer[b] as i32 >> f.bit_lo) & mask
                } else {
                    buffer[b] as i32
                };
                let hat = if raw == f.neutral {
                    0u8
                } else {
                    ((raw + 1) & 0xFF) as u8
                };
                result.insert(sem.to_string(), DecodedValue::Hat(hat));
            }
            FieldOp::Rgb24 => {
                if f.range_lo < 0 {
                    continue;
                }
                let Some(sem) = f.semantic.as_deref() else {
                    continue;
                };
                let lo = f.range_lo as usize;
                if lo + 2 >= buffer.len() {
                    continue;
                }
                result.insert(
                    sem.to_string(),
                    DecodedValue::Bytes(vec![buffer[lo], buffer[lo + 1], buffer[lo + 2]]),
                );
            }
            FieldOp::BytesPassthrough => {
                if f.range_lo < 0 {
                    continue;
                }
                let Some(sem) = f.semantic.as_deref() else {
                    continue;
                };
                if f.range_hi < 0 || f.range_hi as usize >= buffer.len() {
                    continue;
                }
                let lo = f.range_lo as usize;
                let hi = f.range_hi as usize;
                result.insert(
                    sem.to_string(),
                    DecodedValue::Bytes(buffer[lo..=hi].to_vec()),
                );
            }
            FieldOp::Crc32 => {
                let Some(scope) = &f.scope else {
                    continue;
                };
                let dst = if f.crc_dst >= 0 {
                    f.crc_dst
                } else {
                    buffer.len() as i32 - 4
                };
                if dst < 0 || (dst + 3) as usize >= buffer.len() {
                    continue;
                }
                let dst = dst as usize;
                let observed = buffer[dst] as u32
                    | (buffer[dst + 1] as u32) << 8
                    | (buffer[dst + 2] as u32) << 16
                    | (buffer[dst + 3] as u32) << 24;
                let expected = compute_crc32(scope, buffer);
                crc_valid = observed == expected;
            }
            // Unhandled / input-only types: omit from the result, as C# does.
            _ => {}
        }
    }
    (result, crc_valid)
}

#[cfg(test)]
mod tests {
    use super::super::spec::ReportSpec;
    use super::*;

    fn compile(json: &str) -> Program {
        super::super::program::compile(&ReportSpec::from_json(json).expect("parse"))
            .expect("compile")
    }

    fn encode(json: &str, state: &InputState) -> Vec<u8> {
        let program = compile(json);
        let mut buf = vec![0u8; program.size];
        let mut enc = EncoderState::default();
        encode_input(&program, state, &mut buf, &mut enc);
        buf
    }

    #[test]
    fn round_half_even_matches_csharp() {
        assert_eq!(round_half_even(63.5), 64.0); // 63 odd -> up to even
        assert_eq!(round_half_even(64.5), 64.0); // 64 even -> stay
        assert_eq!(round_half_even(127.5), 128.0);
        assert_eq!(round_half_even(-63.5), -64.0);
        assert_eq!(round_half_even(63.75), 64.0);
    }

    #[test]
    fn u8_axis_default_center() {
        // report id at byte 0, axis at byte 1
        let json = r#"{"reportId":"0x01","size":3,
            "fields":[{"byte":1,"type":"uint8-axis","semantic":"leftStickX"}]}"#;
        let axis = |v: f32| {
            encode(
                json,
                &InputState {
                    left_stick_x: v,
                    ..Default::default()
                },
            )[1]
        };
        assert_eq!(axis(0.25), 64); // 63.75 -> 64
        assert_eq!(axis(0.5), 128); // 127.5 -> 128 (even)
        assert_eq!(axis(1.0), 255);
    }

    #[test]
    fn u8_trigger() {
        let json = r#"{"reportId":"0x01","size":3,
            "fields":[{"byte":2,"type":"uint8-trigger","semantic":"rightTrigger"}]}"#;
        let s = InputState {
            right_trigger: 0.75,
            ..Default::default()
        }; // 191.25 -> 191
        assert_eq!(encode(json, &s)[2], 191);
    }

    #[test]
    fn u8_const_and_report_id() {
        let json = r#"{"reportId":"0x31","size":4,
            "fields":[{"byte":3,"type":"uint8","initial":171}]}"#;
        let out = encode(json, &InputState::default());
        assert_eq!(out[0], 0x31, "report id");
        assert_eq!(out[3], 171);
    }

    #[test]
    fn i16_le_gyro_and_accel() {
        let json = r#"{"reportId":"0x01","size":6,
            "fields":[{"byte":1,"type":"int16-le","semantic":"gyroPitch"},
                      {"byte":3,"type":"int16-le","semantic":"accelY"}]}"#;
        let s = InputState {
            gyro_pitch: 1000,
            accel_y: -8192,
            ..Default::default()
        };
        let out = encode(json, &s); // gyro 0x03E8 LE -> E8 03; accel -8192 0xE000 -> 00 E0
        assert_eq!(&out[1..3], &[0xE8, 0x03]);
        assert_eq!(&out[3..5], &[0x00, 0xE0]);
    }

    #[test]
    fn stick12_pair_shares_the_middle_byte() {
        let json = r#"{"reportId":"0x09","size":5,
            "fields":[{"byte":1,"type":"stick12-pair","semantic":"leftStick"}]}"#;
        let s = InputState {
            left_stick_x: 0.5,
            left_stick_y: 0.5,
            ..Default::default()
        }; // 2047.5 -> 2048 (0x800)
        // x=0x800: [0]=0x00, [1]=(0x08)|(0x00<<4)=0x08, [2]=(0x800>>4)&0xFF=0x80
        assert_eq!(&encode(json, &s)[1..4], &[0x00, 0x08, 0x80]);
    }

    #[test]
    fn i16_axis_full_scale_and_y_inversion() {
        let jx = r#"{"reportId":"0x01","size":4,
            "fields":[{"byte":1,"type":"int16-axis","semantic":"leftStickX"}]}"#;
        let jy = r#"{"reportId":"0x01","size":4,
            "fields":[{"byte":1,"type":"int16-axis","semantic":"leftStickY"}]}"#;
        let sx = InputState {
            left_stick_x: 1.0,
            ..Default::default()
        };
        assert_eq!(&encode(jx, &sx)[1..3], &[0xFF, 0x7F]); // +32767 = 0x7FFF LE
        let sy = InputState {
            left_stick_y: 1.0,
            ..Default::default()
        };
        assert_eq!(&encode(jy, &sy)[1..3], &[0x01, 0x80]); // Y inverted -32767 = 0x8001 LE
    }

    #[test]
    fn u8_rolling_advances_by_stride() {
        let json = r#"{"reportId":"0x01","size":3,
            "fields":[{"byte":1,"type":"uint8-rolling","semantic":"seq","initial":16,"stride":16}]}"#;
        let program = compile(json);
        let mut buf = vec![0u8; program.size];
        let mut enc = EncoderState::default();
        let mut seen = Vec::new();
        for _ in 0..3 {
            encode_input(&program, &InputState::default(), &mut buf, &mut enc);
            seen.push(buf[1]);
        }
        assert_eq!(seen, vec![16, 32, 48]);
    }

    // ── Valve state-packet ops (no golden coverage; hand-computed) ────────────

    #[test]
    fn u16_trigger_full_pull_and_banker_half() {
        let json = r#"{"reportId":"0x01","size":4,
            "fields":[{"byte":1,"type":"uint16-trigger","semantic":"leftTrigger"}]}"#;
        let full = InputState {
            left_trigger: 1.0,
            ..Default::default()
        };
        assert_eq!(&encode(json, &full)[1..3], &[0xFF, 0x7F]); // 32767 LE
        let half = InputState {
            left_trigger: 0.5,
            ..Default::default()
        };
        // 0.5 * 32767 = 16383.5 -> 16384 (even) = 0x4000
        assert_eq!(&encode(json, &half)[1..3], &[0x00, 0x40]);
    }

    #[test]
    fn u16_pressure_is_full_scale_on_contact_else_zero() {
        let json = r#"{"reportId":"0x01","size":4,
            "fields":[{"byte":1,"type":"uint16-pressure","semantic":"leftPadPressure"}]}"#;
        let down = InputState {
            finger0_active: true,
            ..Default::default()
        };
        assert_eq!(&encode(json, &down)[1..3], &[0xFF, 0x7F]); // 32767
        let up = InputState {
            finger0_active: false,
            ..Default::default()
        };
        assert_eq!(&encode(json, &up)[1..3], &[0x00, 0x00]);
    }

    #[test]
    fn u32_rolling_advances_across_frames() {
        let json = r#"{"reportId":"0x01","size":6,
            "fields":[{"byte":1,"type":"uint32-rolling","semantic":"pkt","initial":0,"stride":1}]}"#;
        let program = compile(json);
        let mut buf = vec![0u8; program.size];
        let mut enc = EncoderState::default();
        let mut seen = Vec::new();
        for _ in 0..3 {
            encode_input(&program, &InputState::default(), &mut buf, &mut enc);
            seen.push(u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]));
        }
        assert_eq!(seen, vec![0, 1, 2]);
    }

    #[test]
    fn i16_pad_maps_contact_full_scale_with_y_up() {
        let jx = r#"{"reportId":"0x01","size":4,
            "fields":[{"byte":1,"type":"int16-pad","semantic":"leftPadX"}]}"#;
        let sx = InputState {
            finger0_active: true,
            finger0_x: 1919,
            ..Default::default()
        };
        assert_eq!(&encode(jx, &sx)[1..3], &[0xFF, 0x7F]); // +32767

        let jy = r#"{"reportId":"0x01","size":4,
            "fields":[{"byte":1,"type":"int16-pad","semantic":"leftPadY"}]}"#;
        let sy = InputState {
            finger0_active: true,
            finger0_y: 1079,
            ..Default::default()
        };
        assert_eq!(&encode(jy, &sy)[1..3], &[0x01, 0x80]); // Y inverted -> -32767
    }

    #[test]
    fn i16_pad_or_stick_falls_back_to_stick_when_finger_up() {
        let s = InputState {
            finger0_active: false,
            left_stick_x: 1.0,
            ..Default::default()
        };
        let or_stick = r#"{"reportId":"0x01","size":4,
            "fields":[{"byte":1,"type":"int16-pad-or-stick","semantic":"leftPadX"}]}"#;
        assert_eq!(&encode(or_stick, &s)[1..3], &[0xFF, 0x7F]); // reads the stick
        let plain = r#"{"reportId":"0x01","size":4,
            "fields":[{"byte":1,"type":"int16-pad","semantic":"leftPadX"}]}"#;
        assert_eq!(&encode(plain, &s)[1..3], &[0x00, 0x00]); // no fallback -> zero
    }
}

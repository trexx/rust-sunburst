// SPDX-License-Identifier: GPL-2.0-or-later

//! The generic report codec — the single walker that serves every profile.
//!
//! Ported from HIDMaestro's `VendorBlobCodec`. Byte-for-byte parity is the
//! contract: the same input state through the same profile must produce the same
//! wire bytes HIDMaestro's driver expects, or a game rejects the pad. The final
//! proof is the golden-hash test against HIDMaestro's own captured vectors; it
//! arrives once every input op is ported.
//!
//! # Rounding
//!
//! C#'s `Math.Round(double)` is **round-half-to-even** (banker's rounding), not
//! round-half-away-from-zero as Rust's `f32::round` is. A stick at exactly 0.5
//! must land on 128, not 127 or 129, so [`round_half_even`] reproduces it. The
//! scale multiply is done in `f32` first, matching C#'s `float * int` before the
//! promotion to `double` for the round.

use std::collections::HashMap;

use super::program::{CompiledField, FieldOp, Program, SrcOp};

/// The input state one frame carries. Sticks and triggers are `[0, 1]` floats
/// (0.5 = centre), the IMU is raw `i16`, matching what HIDMaestro's encoder
/// takes. Fields an unported op has not needed yet default to neutral.
#[derive(Debug, Clone, Default)]
pub struct InputState {
    pub left_stick_x: f32,
    pub left_stick_y: f32,
    pub right_stick_x: f32,
    pub right_stick_y: f32,
    pub left_trigger: f32,
    pub right_trigger: f32,
    pub gyro_pitch: i16,
    pub gyro_yaw: i16,
    pub gyro_roll: i16,
    pub accel_x: i16,
    pub accel_y: i16,
    pub accel_z: i16,
    pub sensor_timestamp: u32,
}

/// State that persists across frames: the rolling counters, keyed as the
/// compiler precomputed.
#[derive(Debug, Default)]
pub struct EncoderState {
    rolling: HashMap<String, u8>,
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
/// Faithful to `VendorBlobCodec.EncodeInput`. Ops not yet ported fall through to
/// the `Unknown` no-op, which is why real profiles are gated behind the golden
/// test rather than used before it passes.
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
            FieldOp::U8Const if f.byte >= 0 => buffer[f.byte as usize] = f.initial,
            FieldOp::I16 => i16_le(f, state, buffer),
            FieldOp::I16Axis => i16_axis(f, state, buffer),
            // Not yet ported (or genuinely unknown): silent no-op, HIDMaestro's
            // own contract for an unrecognised type.
            _ => {}
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
    let sraw = (round_half_even((centred * 2.0 * 32767.0) as f64) as i32).clamp(-32767, 32767) as i16;
    let b = b as usize;
    buffer[b] = (sraw & 0xFF) as u8;
    buffer[b + 1] = ((sraw >> 8) & 0xFF) as u8;
}

#[cfg(test)]
mod tests {
    use super::super::spec::ReportSpec;
    use super::*;

    fn compile(json: &str) -> Program {
        super::super::program::compile(&ReportSpec::from_json(json).expect("parse")).expect("compile")
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
            encode(json, &InputState { left_stick_x: v, ..Default::default() })[1]
        };
        assert_eq!(axis(0.25), 64); // 63.75 -> 64
        assert_eq!(axis(0.5), 128); // 127.5 -> 128 (even)
        assert_eq!(axis(1.0), 255);
    }

    #[test]
    fn u8_trigger() {
        let json = r#"{"reportId":"0x01","size":3,
            "fields":[{"byte":2,"type":"uint8-trigger","semantic":"rightTrigger"}]}"#;
        let s = InputState { right_trigger: 0.75, ..Default::default() }; // 191.25 -> 191
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
        let s = InputState { gyro_pitch: 1000, accel_y: -8192, ..Default::default() };
        let out = encode(json, &s); // gyro 0x03E8 LE -> E8 03; accel -8192 0xE000 -> 00 E0
        assert_eq!(&out[1..3], &[0xE8, 0x03]);
        assert_eq!(&out[3..5], &[0x00, 0xE0]);
    }

    #[test]
    fn stick12_pair_shares_the_middle_byte() {
        let json = r#"{"reportId":"0x09","size":5,
            "fields":[{"byte":1,"type":"stick12-pair","semantic":"leftStick"}]}"#;
        let s = InputState { left_stick_x: 0.5, left_stick_y: 0.5, ..Default::default() }; // 2047.5 -> 2048 (0x800)
        // x=0x800: [0]=0x00, [1]=(0x08)|(0x00<<4)=0x08, [2]=(0x800>>4)&0xFF=0x80
        assert_eq!(&encode(json, &s)[1..4], &[0x00, 0x08, 0x80]);
    }

    #[test]
    fn i16_axis_full_scale_and_y_inversion() {
        let jx = r#"{"reportId":"0x01","size":4,
            "fields":[{"byte":1,"type":"int16-axis","semantic":"leftStickX"}]}"#;
        let jy = r#"{"reportId":"0x01","size":4,
            "fields":[{"byte":1,"type":"int16-axis","semantic":"leftStickY"}]}"#;
        let sx = InputState { left_stick_x: 1.0, ..Default::default() };
        assert_eq!(&encode(jx, &sx)[1..3], &[0xFF, 0x7F]); // +32767 = 0x7FFF LE
        let sy = InputState { left_stick_y: 1.0, ..Default::default() };
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
}

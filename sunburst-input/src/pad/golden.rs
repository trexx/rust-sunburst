// SPDX-License-Identifier: GPL-2.0-or-later

//! The byte-parity proof: reproduce HIDMaestro's own golden report hashes.
//!
//! HIDMaestro locks its codec's wire format with
//! `test/probes/vendor_blob_golden_check`: for every shipped profile it encodes
//! three frames of a fully-populated, deterministic gamepad state, SHA-256 hashes
//! each report, and compares against a committed golden table (captured from the
//! pre-refactor string-switch codec, regenerated only on a deliberate wire-format
//! change). If a hash diverges, the codec has drifted from the bytes a real
//! device emits — which a game would reject.
//!
//! This test is the Rust port's half of that contract. It consumes the **same**
//! nine Sony profile JSONs (vendored verbatim under `third-party/hidmaestro/`),
//! builds the **same** `MakeState(frame)` and the same six stick/trigger floats
//! HIDMaestro's probe uses, and asserts the Rust codec produces byte-for-byte the
//! same reports — proven by matching HIDMaestro's committed input-direction
//! hashes. It runs on Linux: no driver, no device, no .NET.
//!
//! Golden source: HIDMaestro v1.8.0 (`dcdf9b48`),
//! `test/probes/vendor_blob_golden_check/Program.cs`, the `in` rows.

use std::collections::{BTreeMap, HashMap};

use super::codec::{
    DecodedValue, EncoderState, InputState, OutValue, decode, encode_input, encode_output,
};
use super::program::{Hat, compile};
use super::spec::{ReportSpec, parse_range};

/// A profile JSON, of which the codec reads the input and output report blocks.
/// serde ignores every other key (`descriptor`, `buttonMap`, …).
#[derive(serde::Deserialize)]
struct Profile {
    #[serde(rename = "extendedReport")]
    extended_report: Option<ReportSpec>,
    #[serde(rename = "extendedOutputReport")]
    extended_output_report: Option<ReportSpec>,
}

/// The nine vendored Sony profiles the proof covers, `include_str!`d so the test
/// is hermetic — the same bytes CI checks are the bytes on disk.
const PROFILES: &[(&str, &str)] = &[
    (
        "dualsense",
        include_str!("../../../third-party/hidmaestro/profiles/sony/dualsense.json"),
    ),
    (
        "dualsense-edge",
        include_str!("../../../third-party/hidmaestro/profiles/sony/dualsense-edge.json"),
    ),
    (
        "dualsense-bt",
        include_str!("../../../third-party/hidmaestro/profiles/sony/dualsense-bt.json"),
    ),
    (
        "dualsense-bt-full",
        include_str!("../../../third-party/hidmaestro/profiles/sony/dualsense-bt-full.json"),
    ),
    (
        "dualsense-edge-bt",
        include_str!("../../../third-party/hidmaestro/profiles/sony/dualsense-edge-bt.json"),
    ),
    (
        "dualshock-4-v1",
        include_str!("../../../third-party/hidmaestro/profiles/sony/dualshock-4-v1.json"),
    ),
    (
        "dualshock-4-v1-full",
        include_str!("../../../third-party/hidmaestro/profiles/sony/dualshock-4-v1-full.json"),
    ),
    (
        "dualshock-4-v2",
        include_str!("../../../third-party/hidmaestro/profiles/sony/dualshock-4-v2.json"),
    ),
    (
        "dualshock-4-v2-bt",
        include_str!("../../../third-party/hidmaestro/profiles/sony/dualshock-4-v2-bt.json"),
    ),
];

/// HIDMaestro's committed input-direction goldens: `(profile, [frame0, 1, 2])`.
/// Copied from the `golden <id> in <frame>` rows of its probe at v1.8.0.
const GOLDEN_IN: &[(&str, [&str; 3])] = &[
    (
        "dualsense",
        [
            "C96204DDB3A365463A63D49FA6D499C6F32F9CEBB355D6AF05C856EE67E2DA02",
            "D992926129BF5C759277C3994751EE4BA844E0329E37F8302C5DB74656EB2CCC",
            "BAB51D2F427459ED88CFEDDDDEC43EB748FEFFD6F822EF750C5C4136C957026C",
        ],
    ),
    (
        "dualsense-edge",
        [
            "D241BAFE24815AD5EBF7B58E4F4E924078EA3CDBE9662919A5842DA50B15E4E0",
            "36FC5056BA650235C7BB456EB2DDBE5C9991ED6253A8995A24F07CBE0CC74ED8",
            "BAB51D2F427459ED88CFEDDDDEC43EB748FEFFD6F822EF750C5C4136C957026C",
        ],
    ),
    (
        "dualsense-bt",
        [
            "2706C4CF2E04BECA85F5D00F4378DDA7022CB1BCDD683B3A4F98A654649A898D",
            "7698D309C8718A383C2A1C085F64C8985C0B8F8849B5790E973B88772FD0BC9A",
            "91E35509678306C5389B0923795A53EB35D35F6672EE0DDAE9B94478981FBBA2",
        ],
    ),
    (
        "dualsense-bt-full",
        [
            "2706C4CF2E04BECA85F5D00F4378DDA7022CB1BCDD683B3A4F98A654649A898D",
            "7698D309C8718A383C2A1C085F64C8985C0B8F8849B5790E973B88772FD0BC9A",
            "91E35509678306C5389B0923795A53EB35D35F6672EE0DDAE9B94478981FBBA2",
        ],
    ),
    (
        "dualsense-edge-bt",
        [
            "4F4CD3CB5044E37F731AAEA0D0FE905732A9175F6EA3EFDC220282FCBD8E876B",
            "92D3AD0292B3462238CA3B019B1C9FFDF129A5D9707FDD1F904B1C803A165C48",
            "74520D79DE2C6BFAD440E89F5B1D14B6004220E663D19A425ADD155AED7D76BF",
        ],
    ),
    (
        "dualshock-4-v1",
        [
            "A8247649E5164EA0A2B6F100181C58DD7D509260BF53B385E304A127D121C4F3",
            "BC2FBE5805E003D1CF14551C50E3A4B6CD67F0674398917B6B8E07660762D30A",
            "4984027A7876BF52D3572032396A47FA9D3E91D20CF2AD1202EFA70FA4F60216",
        ],
    ),
    (
        "dualshock-4-v1-full",
        [
            "A8247649E5164EA0A2B6F100181C58DD7D509260BF53B385E304A127D121C4F3",
            "BC2FBE5805E003D1CF14551C50E3A4B6CD67F0674398917B6B8E07660762D30A",
            "4984027A7876BF52D3572032396A47FA9D3E91D20CF2AD1202EFA70FA4F60216",
        ],
    ),
    (
        "dualshock-4-v2",
        [
            "A8247649E5164EA0A2B6F100181C58DD7D509260BF53B385E304A127D121C4F3",
            "BC2FBE5805E003D1CF14551C50E3A4B6CD67F0674398917B6B8E07660762D30A",
            "4984027A7876BF52D3572032396A47FA9D3E91D20CF2AD1202EFA70FA4F60216",
        ],
    ),
    (
        "dualshock-4-v2-bt",
        [
            "435C508CEFA9FDADE7115AE5B17BE4AAB58538B9A109FB37EDA937E4E85F8DF7",
            "1560AF04076712D1DC8CF37210981E6894A90E5A84BD1E42D0AB07968B8BA3F0",
            "045747EDEE30B03BCFB8B46E72B625E87F3E33BCAEAD732583B30F69658F64DF",
        ],
    ),
];

/// HIDMaestro's `MakeState(frame)` — the deterministic, fully-populated state
/// that exercises every input field type across the nine specs. Every cast
/// mirrors the C# (`(short)`, `(ushort)`, `(uint)`) so the wire bytes match.
///
/// The six stick/trigger floats are passed separately in HIDMaestro (its
/// `HMController.SubmitState` pre-resolves them from the profile); here they live
/// on [`InputState`] and are set the same way.
fn make_state(frame: i32) -> InputState {
    InputState {
        // Sticks/triggers: HIDMaestro's EncodeInput float arguments, verbatim.
        left_stick_x: 0.25f32 + frame as f32 * 0.1f32,
        left_stick_y: 0.75f32,
        right_stick_x: 0.40f32,
        right_stick_y: 0.60f32,
        left_trigger: 0.10f32 + frame as f32 * 0.2f32,
        right_trigger: 0.90f32,

        buttons: 0x0000_A5A5u32 ^ (frame as u32).wrapping_mul(0x1111),
        hat: Hat::NorthEast,

        gyro_pitch: (1000 + frame * 17) as i16,
        gyro_yaw: (-2000 + frame * 13) as i16,
        gyro_roll: (300 - frame * 7) as i16,
        accel_x: (4096 + frame) as i16,
        accel_y: (-8192 + frame * 3) as i16,
        accel_z: (512 + frame * 5) as i16,
        sensor_timestamp: 0xDEAD_0000u32.wrapping_add((frame as u32).wrapping_mul(1333)),

        finger0_active: true,
        finger0_x: (960 + frame * 10) as u16,
        finger0_y: (540 - frame * 10) as u16,
        finger0_id: (1 + frame) as u8,
        finger1_active: frame != 1,
        finger1_x: 100,
        finger1_y: 200,
        finger1_id: 7,

        battery_level: 8,
        battery_charging: true,
        battery_full: false,
        mic_muted: frame == 2,
        headphones_connected: true,
    }
}

#[test]
fn input_reports_match_hidmaestro_golden_hashes() {
    // Index the vendored profiles by id.
    let mut failures = Vec::new();
    let mut checked = 0;

    for (id, expected) in GOLDEN_IN {
        let json = PROFILES
            .iter()
            .find(|(pid, _)| pid == id)
            .map(|(_, j)| *j)
            .unwrap_or_else(|| panic!("vendored profile {id} missing"));
        let profile: Profile =
            serde_json::from_str(json).unwrap_or_else(|e| panic!("parse {id}: {e}"));
        let spec = profile
            .extended_report
            .unwrap_or_else(|| panic!("{id} has no extendedReport"));
        let program = compile(&spec).unwrap_or_else(|e| panic!("compile {id}: {e}"));

        // One EncoderState per profile, three successive frames — the rolling
        // counters must advance across frames exactly as HIDMaestro's do.
        let mut enc = EncoderState::default();
        let mut buf = vec![0u8; program.size];
        for (frame, exp) in expected.iter().enumerate() {
            let state = make_state(frame as i32);
            encode_input(&program, &state, &mut buf, &mut enc);
            let got = hex_upper(&sha256(&buf));
            if got != *exp {
                failures.push(format!("{id} in {frame}: got {got}, expected {exp}"));
            }
            checked += 1;
        }
    }

    assert_eq!(checked, 27, "expected 27 input-direction golden checks");
    assert!(
        failures.is_empty(),
        "golden mismatches:\n{}",
        failures.join("\n")
    );
}

// ── Output direction + decode ─────────────────────────────────────────────────

/// HIDMaestro's committed output-encode goldens: `(profile, [frame0, 1, 2])`.
const GOLDEN_OUT: &[(&str, [&str; 3])] = &[
    (
        "dualsense",
        [
            "83807013E95889568F83AFD0DDD4BA218A9AEA757D8E57FE694D9BEB3A29F17C",
            "D12BD0586374C7816F9474A0FCA296BE2B6E10A98C85894B58E2C4F60F694F5F",
            "A94544952CCCB8A14C0FB46D6424AED8146D3C46DD7A0C79BC26732785552505",
        ],
    ),
    (
        "dualsense-edge",
        [
            "953E8E28585A6BD2D2C45EFE53853CEAD310F32636A61E5C3EF25178571564EC",
            "2ED5B6C68433FEECF72737E28F62310F1AF20A9CB5FACACB13967C0B25CA6D09",
            "EF86AAE9971624DE0319A35D9E8A168B27A96803F9A672FB3106152B2C172774",
        ],
    ),
    (
        "dualsense-bt",
        [
            "6816CBEC199754791F3D942FA95456760291063D4A92DA99014B32687B24D333",
            "F6CA9E6EE5D8C3771812E3D931D5314471EFD0AB438A351E1807F72C641DC9BF",
            "1896ACEA1C3C8709891E9B9AB50F97AD7616709625E887353E4572F4600349F4",
        ],
    ),
    (
        "dualsense-bt-full",
        [
            "6816CBEC199754791F3D942FA95456760291063D4A92DA99014B32687B24D333",
            "F6CA9E6EE5D8C3771812E3D931D5314471EFD0AB438A351E1807F72C641DC9BF",
            "1896ACEA1C3C8709891E9B9AB50F97AD7616709625E887353E4572F4600349F4",
        ],
    ),
    (
        "dualsense-edge-bt",
        [
            "6816CBEC199754791F3D942FA95456760291063D4A92DA99014B32687B24D333",
            "F6CA9E6EE5D8C3771812E3D931D5314471EFD0AB438A351E1807F72C641DC9BF",
            "1896ACEA1C3C8709891E9B9AB50F97AD7616709625E887353E4572F4600349F4",
        ],
    ),
    (
        "dualshock-4-v1",
        [
            "4B7A785C65BAA747572BE6787D8CDA352CF3147A6DF1674918B52DFE099F2299",
            "392D0B472E16392FB865637AFE56F0D97D5C8630B64DB2E1671D367A77C489C6",
            "79AB7B18E68124DDD5C19445AE30DCF2D65E6BE5FB70D110A614BAAEF2462C42",
        ],
    ),
    (
        "dualshock-4-v1-full",
        [
            "4B7A785C65BAA747572BE6787D8CDA352CF3147A6DF1674918B52DFE099F2299",
            "392D0B472E16392FB865637AFE56F0D97D5C8630B64DB2E1671D367A77C489C6",
            "79AB7B18E68124DDD5C19445AE30DCF2D65E6BE5FB70D110A614BAAEF2462C42",
        ],
    ),
    (
        "dualshock-4-v2",
        [
            "4B7A785C65BAA747572BE6787D8CDA352CF3147A6DF1674918B52DFE099F2299",
            "392D0B472E16392FB865637AFE56F0D97D5C8630B64DB2E1671D367A77C489C6",
            "79AB7B18E68124DDD5C19445AE30DCF2D65E6BE5FB70D110A614BAAEF2462C42",
        ],
    ),
    (
        "dualshock-4-v2-bt",
        [
            "753BCBF657E30B407A7F4ADC26077B671B58C767F53513967A8FD40E0E90D722",
            "F43E0A404EEE79E47B825691DF71BBBF0A4E52980B5C5BB68CBD22E00E7E9B34",
            "04FA1DE1110169E9A669C3D7B1951F6E3491342F3BDB434F83ABF732FDBE63AF",
        ],
    ),
];

/// HIDMaestro's committed decode goldens: `(profile, hash)` of the canonical dump
/// of the last (frame-2) output report, plus its CRC-valid flag.
const GOLDEN_DEC: &[(&str, &str)] = &[
    (
        "dualsense",
        "52756DE5DB40B971507510186D306B288AD5C352841FE8D253EC995A6143DD40",
    ),
    (
        "dualsense-edge",
        "52756DE5DB40B971507510186D306B288AD5C352841FE8D253EC995A6143DD40",
    ),
    (
        "dualsense-bt",
        "69D6D7461B7B3D7307FFB3E361681753507849B1D02BC78AF7B323A82C9A5BAB",
    ),
    (
        "dualsense-bt-full",
        "69D6D7461B7B3D7307FFB3E361681753507849B1D02BC78AF7B323A82C9A5BAB",
    ),
    (
        "dualsense-edge-bt",
        "69D6D7461B7B3D7307FFB3E361681753507849B1D02BC78AF7B323A82C9A5BAB",
    ),
    (
        "dualshock-4-v1",
        "2F0580BB72322624CE3EF9C03BEC29ED21B61C1F6EA2A55CC4D845AC9F43DEE7",
    ),
    (
        "dualshock-4-v1-full",
        "2F0580BB72322624CE3EF9C03BEC29ED21B61C1F6EA2A55CC4D845AC9F43DEE7",
    ),
    (
        "dualshock-4-v2",
        "2F0580BB72322624CE3EF9C03BEC29ED21B61C1F6EA2A55CC4D845AC9F43DEE7",
    ),
    (
        "dualshock-4-v2-bt",
        "BA2EC300DF7B4D788866A9CFCD8FCBBC1597DF71EE115BCD27FE0786CF4FFBFB",
    ),
];

/// HIDMaestro's FNV-1a-based per-semantic byte (`NameByte`). Deterministic filler
/// so each output field carries a distinct, reproducible value.
fn name_byte(s: &str) -> u8 {
    let mut h: u32 = 2166136261;
    for c in s.chars() {
        h = (h ^ c as u32).wrapping_mul(16777619);
    }
    (h & 0x7F) as u8
}

/// Length of a `"lo-hi"` byte range, else the fallback (`RangeLen`).
fn range_len(bytes: Option<&str>, fallback: usize) -> usize {
    match bytes.and_then(parse_range) {
        Some((lo, hi)) => (hi - lo + 1) as usize,
        None => fallback,
    }
}

/// HIDMaestro's `MakeOutputFields`: a deterministic per-semantic value map. Every
/// output field except the auto-advancing rolling counter and the CRC gets a
/// value, so the encoder's rolling and CRC paths are what the golden locks.
fn make_output_fields(spec: &ReportSpec, frame: i32) -> HashMap<String, OutValue> {
    let mut fields = HashMap::new();
    for f in &spec.fields {
        let Some(sem) = f.semantic.as_deref() else {
            continue;
        };
        if f.kind == "crc32-le" || f.kind == "uint8-rolling" {
            continue;
        }
        if fields.contains_key(sem) {
            continue;
        }
        let value = match f.kind.as_str() {
            "rgb24" => OutValue::Bytes(vec![
                (10 + frame) as u8,
                (20 + frame) as u8,
                (30 + frame) as u8,
            ]),
            "bytes-passthrough" => {
                let len = range_len(f.bytes.as_deref(), 4);
                let nb = name_byte(sem);
                OutValue::Bytes(
                    (0..len)
                        .map(|i| nb.wrapping_add(i as u8).wrapping_add(frame as u8))
                        .collect(),
                )
            }
            _ => OutValue::Byte(name_byte(sem).wrapping_add(frame as u8)),
        };
        fields.insert(sem.to_string(), value);
    }
    fields
}

/// HIDMaestro's `Canonical`: an ordinal-ordered `key=value;` dump, blobs as
/// `hex:UPPER`, plus the trailing `crcValid=True/False`. The `BTreeMap` is
/// already in ordinal key order for these ASCII semantics.
fn canonical(decoded: &BTreeMap<String, DecodedValue>, crc_valid: bool) -> String {
    let mut s = String::new();
    for (k, v) in decoded {
        s.push_str(k);
        s.push('=');
        match v {
            DecodedValue::Byte(b) => s.push_str(&b.to_string()),
            DecodedValue::Bytes(arr) => {
                s.push_str("hex:");
                for b in arr {
                    s.push_str(&format!("{b:02X}"));
                }
            }
            other => panic!(
                "canonical: no shipped output spec decodes to {other:?}; \
                 .NET float/list formatting parity is out of this proof's scope"
            ),
        }
        s.push(';');
    }
    s.push_str("crcValid=");
    s.push_str(if crc_valid { "True" } else { "False" });
    s
}

#[test]
fn output_reports_and_decode_match_hidmaestro_golden_hashes() {
    let mut failures = Vec::new();
    let mut checked = 0;

    for (id, expected_out) in GOLDEN_OUT {
        let json = PROFILES
            .iter()
            .find(|(pid, _)| pid == id)
            .map(|(_, j)| *j)
            .unwrap_or_else(|| panic!("vendored profile {id} missing"));
        let profile: Profile =
            serde_json::from_str(json).unwrap_or_else(|e| panic!("parse {id}: {e}"));
        let spec = profile
            .extended_output_report
            .unwrap_or_else(|| panic!("{id} has no extendedOutputReport"));
        let program = compile(&spec).unwrap_or_else(|e| panic!("compile {id}: {e}"));

        // One EncoderState per profile: the btTag rolling counter advances across
        // the three frames exactly as HIDMaestro's does.
        let mut enc = EncoderState::default();
        let mut buf = vec![0u8; program.size];
        for (frame, exp) in expected_out.iter().enumerate() {
            let fields = make_output_fields(&spec, frame as i32);
            encode_output(&program, &fields, &mut buf, &mut enc);
            let got = hex_upper(&sha256(&buf));
            if got != *exp {
                failures.push(format!("{id} out {frame}: got {got}, expected {exp}"));
            }
            checked += 1;
        }

        // Decode the last (frame-2) report and lock its canonical dump.
        let (decoded, crc_valid) = decode(&program, &buf);
        let canon = canonical(&decoded, crc_valid);
        let got = hex_upper(&sha256(canon.as_bytes()));
        let exp_dec = GOLDEN_DEC
            .iter()
            .find(|(pid, _)| pid == id)
            .map(|(_, h)| *h)
            .unwrap_or_else(|| panic!("no decode golden for {id}"));
        if got != exp_dec {
            failures.push(format!(
                "{id} dec 0: got {got}, expected {exp_dec}\n  canonical: {canon}"
            ));
        }
        checked += 1;
    }

    assert_eq!(checked, 36, "expected 27 output + 9 decode golden checks");
    assert!(
        failures.is_empty(),
        "golden mismatches:\n{}",
        failures.join("\n")
    );
}

// ── SHA-256 (FIPS 180-4), self-contained so the proof pulls no crate ──────────

fn hex_upper(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02X}"));
    }
    s
}

fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    // Padding: 0x80, then zeros to 56 mod 64, then the 64-bit big-endian length.
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, wi) in w.iter_mut().enumerate().take(16) {
            *wi = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[test]
fn sha256_matches_known_vectors() {
    // FIPS 180-4 example vectors — the proof's oracle must itself be correct.
    assert_eq!(
        hex_upper(&sha256(b"")),
        "E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855"
    );
    assert_eq!(
        hex_upper(&sha256(b"abc")),
        "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD"
    );
    assert_eq!(
        hex_upper(&sha256(
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
        )),
        "248D6A61D20638B8E5C026930C3E6039A33CE45964FF2167F6ECEDD419DB06C1"
    );
}

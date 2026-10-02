// SPDX-License-Identifier: GPL-2.0-or-later

//! The NvFBC convert kernels are vendored PTX, not placeholders.
//!
//! `cuda_convert.rs` embeds these files and looks the kernels up by name at
//! runtime, so a wrong entry name or a no-op stub only shows up on the 4070 — as
//! a `cuModuleGetFunction` error, or worse, as a black frame that looks like a
//! capture bug. The checked-in files used to be exactly such stubs. This test
//! reads them as text, so it runs on the Linux host although the crate itself is
//! `#![cfg(windows)]`.
//!
//! Regenerate with `.github/workflows/cuda-kernel.yml`, never by hand.

const P010: &str = include_str!("../cuda/argb10_to_p010.ptx");
const NV12: &str = include_str!("../cuda/argb_to_nv12.ptx");

/// The kernel names `cuda_convert.rs` asks `cuModuleGetFunction` for, and the
/// module each lives in.
const ENTRIES: &[(&str, &str)] = &[
    ("argb10_to_p010.ptx", "argb10_to_p010"),
    ("argb_to_nv12.ptx", "argb_to_nv12"),
    ("argb_to_nv12.ptx", "argb_to_nv12_tonemap"),
    // BT.709 P010 for a 10-bit SDR stream (Hello.display_hdr = false).
    ("argb_to_nv12.ptx", "argb10_to_p010_709"),
    ("argb_to_nv12.ptx", "argb10_to_p010_709_tonemap"),
];

/// `(src, src_pitch_words, dst, dst_pitch_elems, width, height, sdr_white_nits,
/// peak)` — the launch in `cuda_convert.rs` passes exactly these eight, to every
/// kernel (only the SDR tonemap kernels read the last two).
const PARAMS: usize = 8;

fn module(file: &str) -> &'static str {
    if file.starts_with("argb10") {
        P010
    } else {
        NV12
    }
}

/// The text of one `.entry`: its parameter list and body, up to the next entry.
fn entry<'a>(ptx: &'a str, name: &str) -> Option<&'a str> {
    let marker = format!(".entry {name}(");
    let start = ptx.find(&marker)? + marker.len();
    let rest = &ptx[start..];
    let end = rest.find(".entry ").unwrap_or(rest.len());
    Some(&rest[..end])
}

#[test]
fn not_placeholders() {
    for (file, ptx) in [("argb10_to_p010.ptx", P010), ("argb_to_nv12.ptx", NV12)] {
        assert!(
            !ptx.contains("PLACEHOLDER"),
            "{file} is still the no-op placeholder; vendor the cuda-kernel.yml artifact"
        );
        assert!(ptx.contains(".version "), "{file} has no PTX .version");
        assert!(ptx.contains(".target "), "{file} has no PTX .target");
    }
}

#[test]
fn entries_exist_with_the_launch_signature() {
    for &(file, name) in ENTRIES {
        let ptx = module(file);
        let body = entry(ptx, name).unwrap_or_else(|| panic!("no entry `{name}` in its module"));
        let params_end = body.find(')').expect("unterminated parameter list");
        let params = body[..params_end].matches(".param").count();
        assert_eq!(
            params, PARAMS,
            "`{name}` takes {params} params, the launch passes {PARAMS}"
        );
    }
}

#[test]
fn entries_do_real_work() {
    // A stub is `ret;` and nothing else. A real convert reads the capture and
    // writes the planes.
    for &(file, name) in ENTRIES {
        let body = entry(module(file), name).unwrap();
        assert!(
            body.contains("ld.global"),
            "`{name}` never reads global memory"
        );
        assert!(
            body.contains("st.global"),
            "`{name}` never writes global memory"
        );
    }
}

#[test]
fn the_pq_table_init_takes_no_params_and_fills_the_table() {
    // cuda_convert.rs launches it with an empty parameter list before the first
    // tonemapped frame; the tonemap kernels read the table it writes.
    let body = entry(NV12, "init_pq_lut").expect("no entry `init_pq_lut`");
    let params_end = body.find(')').expect("unterminated parameter list");
    assert_eq!(body[..params_end].matches(".param").count(), 0);
    assert!(
        body.contains("st.global"),
        "init_pq_lut never writes the table"
    );
    assert!(NV12.contains("pq_lut"), "the module has no pq_lut table");
}

#[test]
fn the_sdr_p010_entries_write_sixteen_bit_samples() {
    // P010 is u16 per sample; an NV12 (u8) body under a P010 name would write
    // half-width planes and look plausible until it reached the TV.
    for name in ["argb10_to_p010_709", "argb10_to_p010_709_tonemap"] {
        let body = entry(NV12, name).unwrap();
        assert!(
            body.contains("st.global.u16"),
            "`{name}` never stores a u16"
        );
        assert!(!body.contains("st.global.u8"), "`{name}` stores bytes");
    }
}

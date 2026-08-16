// SPDX-License-Identifier: GPL-2.0-or-later

//! The probe itself. Split from `main.rs` so the crate still has a `main`
//! on a non-Windows host and stays in the workspace check.

use std::process::ExitCode;

use crate::nvenc::{Guid, Nvenc, Session, caps};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1};
use windows::core::Interface;

pub fn run() -> ExitCode {
    println!("sunburst probe-windows -- Phase 0.1\n");

    report_adapter();
    println!();
    report_nvfbc();
    println!();
    report_nvml();
    println!();

    match report_encoder() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            println!("\nNVENC: FAILED -- {e}");
            println!("Everything above still stands; the codec matrix does not.");
            ExitCode::FAILURE
        }
    }
}

fn report_adapter() {
    println!("== GPU ==");
    match describe_adapter() {
        Ok(desc) => println!("  {desc}"),
        Err(e) => println!("  could not enumerate: {e}"),
    }
}

fn describe_adapter() -> Result<String, String> {
    // SAFETY: standard DXGI entry point; the returned interface is refcounted
    // and released by `windows`.
    let factory: IDXGIFactory1 =
        unsafe { CreateDXGIFactory1() }.map_err(|e| format!("CreateDXGIFactory1: {e}"))?;
    // SAFETY: adapter 0 exists whenever a display adapter does.
    let adapter: IDXGIAdapter1 =
        unsafe { factory.EnumAdapters1(0) }.map_err(|e| format!("EnumAdapters1: {e}"))?;
    // SAFETY: `GetDesc1` writes into a caller-owned, zeroed struct.
    let desc = unsafe { adapter.GetDesc1() }.map_err(|e| format!("GetDesc1: {e}"))?;

    let name = String::from_utf16_lossy(&desc.Description)
        .trim_end_matches('\0')
        .to_string();
    Ok(format!(
        "{name} (vendor {:04x}, device {:04x}, {} MiB dedicated)",
        desc.VendorId,
        desc.DeviceId,
        desc.DedicatedVideoMemory / (1024 * 1024)
    ))
}

fn report_nvfbc() {
    println!("== NvFBC (ROADMAP 0.1) ==");
    match crate::nvfbc::probe() {
        crate::nvfbc::NvfbcStatus::Available { dll } => {
            println!("  AVAILABLE via {dll}");
            println!("  -> NvFBC becomes the priority-1 capture backend.");
            println!("     Worth more here than on a Ti: encode time is a fixed floor with");
            println!("     one NVENC and no SFE, so DWM composition is the biggest target left.");
        }
        crate::nvfbc::NvfbcStatus::DllWithoutEntryPoint { dll } => {
            println!("  {dll} loaded but has no NvFBCCreateInstance.");
            println!("  -> Treat as unavailable; this looks like a much older Capture SDK.");
        }
        crate::nvfbc::NvfbcStatus::Unavailable => {
            println!("  UNAVAILABLE -- no NvFBC runtime found.");
            println!("  -> The expected answer. Delete the backend from the plan; the Capture");
            println!("     trait must be shaped so its absence costs nothing.");
        }
    }
}

fn report_nvml() {
    println!("== Other NVENC sessions (CLAUDE.md startup warning) ==");
    match crate::nvml::encoder_sessions() {
        Ok(stats) => {
            println!(
                "  {} session(s), {} fps average, {} us average latency",
                stats.session_count, stats.average_fps, stats.average_latency_us
            );
            if stats.session_count > 0 {
                println!("  WARNING: something already holds the encoder -- ShadowPlay, Instant");
                println!("  Replay or OBS. The driver time-shares one physical NVENC, and the");
                println!("  resulting per-frame jitter looks exactly like a bug in our code.");
            }
        }
        Err(e) => println!("  could not tell: {e}"),
    }
}

fn report_encoder() -> Result<(), String> {
    println!("== NVENC ==");

    let nvenc = Nvenc::load()?;
    let (major, minor) = crate::nvenc::decode_driver_version(nvenc.driver_max_version);
    println!("  Driver supports NVENC API {major}.{minor}");
    if crate::nvenc::driver_is_new_enough(nvenc.driver_max_version) {
        println!("  OK -- at or above the API this build targets.");
    } else {
        println!("  WARNING: below the API this build targets. r570 or newer is required;");
        println!("  older headers carry no AV1 GUIDs, so AV1 answers would be false negatives.");
    }

    // A D3D11 device is only needed to open the session; nothing is rendered.
    let mut device: Option<ID3D11Device> = None;
    // SAFETY: standard D3D11 entry point. Every out parameter is either a valid
    // pointer or None, and the feature-level list outlives the call.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            // No software rasteriser module; the driver type above is hardware.
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    }
    .map_err(|e| format!("D3D11CreateDevice: {e}"))?;
    let device = device.ok_or("D3D11CreateDevice returned no device")?;

    let session = nvenc.open_session(device.as_raw())?;

    let guids = session.encode_guids()?;
    let has = |g: Guid| guids.contains(&g);
    println!(
        "  Codecs: H.264 {}, HEVC {}, AV1 {}",
        yes_no(has(crate::nvenc::NV_ENC_CODEC_H264_GUID)),
        yes_no(has(crate::nvenc::NV_ENC_CODEC_HEVC_GUID)),
        yes_no(has(crate::nvenc::NV_ENC_CODEC_AV1_GUID)),
    );

    println!();
    println!("== Capability matrix (HARDWARE_TESTING.md section 1) ==");
    println!("  {:<28} {:>12} {:>12}", "cap", "HEVC", "AV1");
    println!("  {}", "-".repeat(54));

    let rows: [(&str, u32); 10] = [
        ("ref pic invalidation", caps::SUPPORT_REF_PIC_INVALIDATION),
        ("intra refresh", caps::SUPPORT_INTRA_REFRESH),
        ("subframe readback", caps::SUPPORT_SUBFRAME_READBACK),
        ("10-bit encode", caps::SUPPORT_10BIT_ENCODE),
        ("dynamic bitrate change", caps::SUPPORT_DYN_BITRATE_CHANGE),
        ("yuv444 encode", caps::SUPPORT_YUV444_ENCODE),
        ("width max", caps::WIDTH_MAX),
        ("height max", caps::HEIGHT_MAX),
        ("level max", caps::LEVEL_MAX),
        ("encoder engines", caps::NUM_ENCODER_ENGINES),
    ];

    for (label, cap) in rows {
        let hevc = cap_cell(&session, crate::nvenc::NV_ENC_CODEC_HEVC_GUID, cap, has);
        let av1 = cap_cell(&session, crate::nvenc::NV_ENC_CODEC_AV1_GUID, cap, has);
        println!("  {label:<28} {hevc:>12} {av1:>12}");
    }

    println!();
    interpret(&session, has);
    Ok(())
}

fn cap_cell(session: &Session<'_>, codec: Guid, cap: u32, has: impl Fn(Guid) -> bool) -> String {
    if !has(codec) {
        return "n/a".to_string();
    }
    match session.cap(codec, cap) {
        Ok(v) => v.to_string(),
        Err(_) => "err".to_string(),
    }
}

/// Say what the numbers mean, so the answer does not need re-deriving later.
fn interpret(session: &Session<'_>, has: impl Fn(Guid) -> bool) {
    println!("== What this means ==");

    let av1 = crate::nvenc::NV_ENC_CODEC_AV1_GUID;
    if !has(av1) {
        println!("  AV1 is absent. The Homatics has no working HEVC decoder, so it has no");
        println!("  path at all. Check the driver version above before believing this.");
        return;
    }

    match session.cap(av1, caps::SUPPORT_REF_PIC_INVALIDATION) {
        Ok(0) => {
            println!("  AV1 has NO reference picture invalidation.");
            println!("  -> NACK recovery on the Homatics degrades to RequestIdr, with no HEVC");
            println!("     path to fall back to. This reshapes Phase 4; see ROADMAP 0.4.");
        }
        Ok(_) => println!("  AV1 supports reference invalidation, so NACK recovery works there."),
        Err(e) => println!("  AV1 reference invalidation could not be queried: {e}"),
    }

    match session.cap(av1, caps::SUPPORT_SUBFRAME_READBACK) {
        Ok(0) => {
            println!("  AV1 has NO subframe readback.");
            println!("  -> CLAUDE.md calls this mandatory, not an optimisation: with one NVENC");
            println!("     and no SFE, encode time is a fixed 6-10ms floor and emitting tiles");
            println!("     as they complete is the only way to overlap it with transmit.");
        }
        Ok(_) => println!("  AV1 supports subframe readback, so tiles can be emitted as produced."),
        Err(e) => println!("  AV1 subframe readback could not be queried: {e}"),
    }

    if let Ok(engines) = session.cap(av1, caps::NUM_ENCODER_ENGINES) {
        println!("  {engines} encoder engine(s).");
        if engines < 2 {
            println!("  -> As expected on a non-Ti. No Split Frame Encoding; encode time stays");
            println!("     a fixed floor and subframe readback is how it gets hidden.");
        } else {
            println!("  -> More than one engine. CLAUDE.md assumes exactly one; if this is");
            println!("     real, Split Frame Encoding is back on the table and worth revisiting.");
        }
    }
}

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

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

pub fn run(attempt_enable: bool) -> ExitCode {
    println!("sunburst probe-windows -- Phase 0.1\n");

    report_adapter();
    println!();
    report_nvfbc(attempt_enable);
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

/// Enough grabs for a stable p99 without making the probe slow. At the ~4ms a
/// 4K sysmem readback costs, this is a window of roughly two seconds.
const GRAB_COUNT: u32 = 500;

/// Blocking grabs wait for the display, so fewer of them still spans a useful
/// window and the probe does not sit there for ten seconds.
const BLOCKING_COUNT: u32 = 300;

/// Unique frames below which the throughput comparison means nothing.
///
/// A 60Hz desktop with something moving on it produces tens of unique frames in
/// the window above. Two means the desktop sat still, and both capture paths
/// then report how often the screen changed rather than how fast they can go.
/// The first run printed a confident verdict off two frames; this is the guard
/// that should have stopped it.
const MIN_UNIQUE_FOR_VERDICT: usize = 20;

/// Poll, block, and compare against DDA, for one pixel format, through ToCuda.
///
/// This is the interface that matters: the frame stays on the GPU, so the number
/// is not dominated by a sysmem copy the real pipeline would never want.
fn measure_cuda(cuda: &crate::cuda::Cuda, label: &str, ten_bit: bool, hdr: bool) {
    println!();
    let Some(mut session) = crate::tocuda::open(cuda, ten_bit, hdr) else {
        return;
    };
    let polled = crate::tocuda::run(&mut session, GRAB_COUNT, false);
    crate::capture::report(&format!("{label} poll "), &polled);
    let blocked = crate::tocuda::run(&mut session, BLOCKING_COUNT, true);
    crate::capture::report(&format!("{label} block"), &blocked);

    if ten_bit {
        if blocked.is_hdr || polled.is_hdr {
            println!("      bIsHDR set: the desktop is in HDR and NvFBC captured it.");
            println!("      The buffer is A2B10G10R10, NOT the scRGB FP16 CLAUDE.md's shader");
            println!("      notes assume, so the shader's input stage would change.");
        } else {
            println!("      bIsHDR clear -- the desktop is most likely not in HDR mode. The");
            println!("      ToCuda bit position is read from the header, not inferred, so");
            println!("      unlike the ToSys path that is not a candidate explanation.");
        }
    }

    // Release before measuring DDA: holding a live NvFBC session while timing
    // the control would be measuring them together, not against each other.
    drop(session);

    if polled.grabs > 0 {
        interpret_capture("ToCuda", &polled, &blocked);
    }
}

/// The same, through ToSys. Kept for the record; not run unless asked.
fn measure_sys(label: &str, variant: crate::tosys::Variant, ten_bit: bool, hdr: bool) {
    println!();
    let Some(mut session) = crate::tosys::open(variant, ten_bit, hdr) else {
        return;
    };
    let polled = crate::tosys::run(&mut session, GRAB_COUNT, false);
    crate::capture::report(&format!("{label} poll "), &polled);
    let blocked = crate::tosys::run(&mut session, BLOCKING_COUNT, true);
    crate::capture::report(&format!("{label} block"), &blocked);

    if ten_bit {
        if blocked.is_hdr || polled.is_hdr {
            println!("      bIsHDR set: the desktop is in HDR and NvFBC captured it.");
            println!("      The buffer is A2B10G10R10, NOT the scRGB FP16 CLAUDE.md's shader");
            println!("      notes assume, so the shader's input stage would change.");
        } else {
            println!("      bIsHDR clear -- either the desktop is not in HDR mode, or the");
            println!("      request never landed (the V2 bit position is inferred).");
        }
    }

    // Release before measuring DDA: holding a live NvFBC session while timing
    // the control would be measuring them together, not against each other.
    drop(session);

    if polled.grabs > 0 {
        interpret_capture("ToSys", &polled, &blocked);
    }
}

/// The question the capture run exists to answer.
///
/// Deliberately comparative. An NvFBC frame rate on its own says nothing, since
/// a static desktop produces few unique frames however fast the calls return —
/// so this runs DDA over the same wall-clock window and compares.
fn interpret_capture(
    iface: &str,
    polled: &crate::capture::Capture,
    blocked: &crate::capture::Capture,
) {
    // A static desktop yields few unique frames however fast the calls return,
    // so unique-per-second is the honest rate, not raw call rate.
    let unique_fps = blocked.unique as f64 * 1e9 / blocked.elapsed_ns.max(1) as f64;
    let polled_unique_fps = polled.unique as f64 * 1e9 / polled.elapsed_ns.max(1) as f64;
    println!();
    println!("      polling : {polled_unique_fps:.1} unique frames/sec");
    println!("      blocking: {unique_fps:.1} unique frames/sec  <- NvFBC's actual ceiling");
    println!("      The gap between them is wasted work: a poll that finds no new frame");
    println!("      still pays the full copy, so only the blocking figure is comparable.");

    // True regardless of how the comparison lands, and a cost the pipeline would
    // pay on every frame.
    println!(
        "      Per-grab cost {:.2}ms p50 / {:.2}ms p99 -- the sysmem readback, paid",
        polled.p50_ns as f64 / 1e6,
        polled.p99_ns as f64 / 1e6
    );
    println!("      whether or not the frame is new.");

    println!();
    println!("  DDA over the same window, as the control:");
    let dda = match crate::dda::run(blocked.elapsed_ns) {
        Ok(dda) => dda,
        Err(e) => {
            println!("      unavailable -- {e}");
            println!("      Without the control the NvFBC number above is not interpretable.");
            return;
        }
    };
    println!(
        "      {:.1} new frames/sec over {} acquire attempts",
        dda.fps(),
        dda.attempts
    );

    println!();
    if blocked.unique < MIN_UNIQUE_FOR_VERDICT {
        println!(
            "      -> INCONCLUSIVE. Only {} unique frame(s) in the window, so both",
            blocked.unique
        );
        println!("         numbers measure how often the screen changed, not how fast either");
        println!("         path can capture. No verdict is drawn from this.");
        println!();
        if blocked.unique <= 2 && dda.fps() > 10.0 {
            println!(
                "         NOTE: DDA saw {:.0} new frames/sec over the same window, so the",
                dda.fps()
            );
            println!("         screen was NOT static -- this capture path is frozen while the");
            println!("         desktop moves. On an HDR desktop that is what 8-bit ARGB does:");
            println!("         it returns one stale frame forever rather than failing. Use");
            println!("         ARGB10 when the desktop is in HDR.");
        } else {
            println!("         Re-run with something animating full-screen -- a video, or a game");
            println!("         in Big Picture. An idle desktop cannot answer this.");
        }
        return;
    }

    let ratio = unique_fps / dda.fps().max(0.001);
    if ratio >= 1.5 {
        println!("      -> NvFBC {iface} delivered {ratio:.1}x DDA, blocking against blocking.");
        println!("         It is not refresh-capped, so it is not composition-bound, and");
        println!("         CLAUDE.md's ~16.7ms DWM composition line is REAL for this path.");
    } else {
        println!("      -> NvFBC {iface} delivered {ratio:.2}x DDA over the same window, with");
        println!("         both producing enough frames for that to mean something. It does");
        println!("         not beat DDA here.");
    }
    println!();
    println!("         Scope: a throughput comparison, not a latency one. It settles whether");
    println!("         NvFBC delivers more frames than DDA. It does NOT settle what DWM");
    println!("         composition costs -- and if DDA's own rate is well under the display's");
    println!("         refresh, the content was the limit and neither path was stressed.");
}

fn report_nvfbc(attempt_enable: bool) {
    use crate::nvfbc::{Generation, result_name};

    println!("== NvFBC (ROADMAP 0.1) ==");
    let report = crate::nvfbc::probe(attempt_enable);

    let Some(dll) = report.dll else {
        println!("  UNAVAILABLE -- no NvFBC runtime found.");
        println!("  -> Delete the backend from the plan; the Capture trait must be");
        println!("     shaped so its absence costs nothing.");
        return;
    };
    println!("  {dll} loaded.");
    if report.proxy_shim_present {
        println!("  NOTE: NvFBC64_.dll also present, so a proxy shim is installed.");
        println!("        Anything below may be the shim's doing rather than this probe's.");
    }

    match report.generation {
        Generation::Neither => {
            println!("  Exports neither NvFBCCreateInstance nor NvFBC_CreateEx.");
            println!("  -> Not an NvFBC runtime we know. Treat as unavailable.");
            return;
        }
        Generation::Modern => {
            println!("  Exports NvFBCCreateInstance -- the 7.x/Linux-shaped API.");
            println!("  -> Unexpected on Windows. The keyed legacy path below does not apply;");
            println!("     this one takes a function table and needs its own probe.");
            return;
        }
        Generation::Legacy => {
            println!("  Legacy Windows API: {}", report.exports.join(", "));
        }
    }

    if let Some(v) = report.sdk_version {
        println!("  NvFBC_GetSDKVersion: {v} (0x{v:x})");
    }
    if let Some(code) = report.enable {
        println!("  NvFBC_Enable(ENABLE): {} ({code})", result_name(code));
    }

    for (label, status) in [
        ("unkeyed", &report.status_plain),
        ("keyed  ", &report.status_keyed),
    ] {
        if let Some(s) = status {
            println!(
                "  GetStatusEx {label}: {} -- capture_possible {}, capturing {}, \
multi_head {}, cfg_diffmap {}, classification {}, iface v{}",
                result_name(s.result),
                s.capture_possible as u8,
                s.currently_capturing as u8,
                s.multi_head as u8,
                s.configurable_diffmap as u8,
                s.image_classification as u8,
                s.nvfbc_version,
            );
        }
    }

    for (label, create) in [
        ("unkeyed", &report.create_plain),
        ("keyed  ", &report.create_keyed),
    ] {
        if let Some(c) = create {
            println!(
                "  CreateEx    {label}: {} -- object {}, max {}x{}",
                result_name(c.result),
                c.succeeded as u8,
                c.max_width,
                c.max_height,
            );
        }
    }

    let worked = |c: &Option<crate::nvfbc::Create>| c.as_ref().is_some_and(|c| c.succeeded);
    // Decide the verdict from the create results *before* anything consumes
    // them. An earlier version took them first and then tested the emptied
    // Options, which reported "CreateEx refused" directly under a line saying it
    // succeeded.
    let created_unkeyed = worked(&report.create_plain);
    let created_keyed = worked(&report.create_keyed);
    let status_says_possible = report
        .status_keyed
        .as_ref()
        .is_some_and(|s| s.capture_possible);

    let mut shape = None;
    if created_unkeyed || created_keyed {
        println!();
        println!("== NvFBC capture ==");
        // Detection released its own sessions, because NvFBC hands out one at a
        // time; each attempt below likewise lives only as long as it is needed.
        if let Some((session, variant)) = crate::tosys::probe_variants(false, false) {
            println!("  SetUp accepted with: {}", variant.name().trim());
            drop(session);
            shape = Some(variant);
        } else {
            println!("  No setup shape was accepted. The capture question stays open;");
            println!("  the vtable dump above says whether the calls even reached NvFBC.");
        }
    }

    // Both formats get the full treatment, because which one is meaningful
    // depends on a desktop state the probe does not control. With HDR on, the
    // 8-bit path returns a single frozen frame; with it off, ARGB10 is the odd
    // one out. Measuring only one would silently measure the wrong one.
    if shape.is_some() {
        println!();
        println!("== NvFBC capture: ToCuda (frame stays on the GPU) ==");
        match crate::cuda::Cuda::load() {
            Ok(cuda) => {
                for (name, how) in &cuda.resolved {
                    println!("    {name}: resolved via {how}");
                }
                let status = cuda.init();
                if status == crate::cuda::CUDA_SUCCESS {
                    measure_cuda(&cuda, "ARGB  ", false, false);
                    measure_cuda(&cuda, "ARGB10", true, true);
                } else {
                    println!("    cuInit failed with {status}; no CUDA capture possible.");
                }
            }
            Err(e) => println!("    {e}"),
        }
    }

    // ToSys is the path with the 3.6ms sysmem copy. Its numbers are already
    // recorded in HARDWARE_TESTING.md section 1 and it is not what any pipeline
    // would use, so it costs nothing on a normal run.
    if let Some(variant) = shape
        && std::env::args().any(|a| a == "--tosys")
    {
        println!();
        println!("== NvFBC capture: ToSys (copies to system memory) ==");
        measure_sys("ARGB  ", variant, false, false);
        measure_sys("ARGB10", variant, true, true);
    }

    println!();
    if created_unkeyed {
        println!("  -> NvFBC works with no key at all, so this card is not gated.");
        println!("     It becomes a candidate priority-1 backend on its own merits.");
    } else if created_keyed {
        println!("  -> NvFBC is present and switched OFF, not absent: the same call that");
        println!("     fails unkeyed succeeds carrying the private-data key. That is the");
        println!("     evidence the key is what mattered -- a keyed-only run proves nothing.");
        println!("     What it licenses is a measurement, not a backend. See");
        println!("     HARDWARE_TESTING.md section 1 before building on it.");
    } else if status_says_possible {
        println!("  -> Status says capture is possible but CreateEx still refused. Worth a");
        println!("     re-run with --enable-nvfbc, which is the step that needs elevation.");
    } else {
        println!("  -> Unavailable on this driver even with the key. Delete the backend");
        println!("     from the plan; the Capture trait must survive its absence anyway.");
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
        println!("  WARNING: below the API this build targets. Update the driver until it");
        println!("  reports at least that; older headers carry no AV1 GUIDs, so every AV1");
        println!("  answer below would be a false negative rather than an error.");
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
            println!("  -> As expected on AD104. No Split Frame Encoding; encode time stays");
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

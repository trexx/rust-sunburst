// SPDX-License-Identifier: GPL-2.0-or-later

//! Pick a backend and build it, with the WGC↔DDA cross-fallback.
//!
//! WGC is the Win11 default, DDA the Win10 default — they measure within half a
//! millisecond of each other, so the choice is about compatibility, and either
//! falls back to the other if it refuses to construct. NvFBC is built only when
//! explicitly asked for; if it is unavailable the build falls through to the OS
//! default rather than failing outright.

use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};

use crate::dda::DdaCapture;
use crate::tocuda::NvFbcCapture;
use crate::wgc::WgcCapture;
use crate::{Backend, Capture, CaptureError, default_backend};

/// The first Windows 11 build number.
const WIN11_BUILD: u32 = 22000;

/// Whether the host is Windows 11 (build ≥ 22000).
///
/// `RtlGetVersion` is used rather than `GetVersionEx`, which lies to unmanifested
/// processes — it caps at 6.2 without an explicit compatibility manifest.
pub fn is_win11() -> bool {
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a valid, sized OSVERSIONINFOW; RtlGetVersion only writes
    // into it and returns an NTSTATUS.
    let status = unsafe { RtlGetVersion(&mut info) };
    status.is_ok() && info.dwBuildNumber >= WIN11_BUILD
}

/// Set per-monitor-v2 DPI awareness. Without it, capture coordinates are wrong on
/// scaled displays. Idempotent and best-effort — a process that already declared
/// awareness (via a manifest) fails here harmlessly.
pub fn set_dpi_awareness() {
    // SAFETY: a documented context constant; no pointers involved.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Build the capture backend for this host.
///
/// `nvfbc_opt_in` selects NvFBC (falling through to the OS default if it is
/// unavailable). `hdr` is a hint the NvFBC path uses to pick ARGB10 vs ARGB8;
/// DDA and WGC detect HDR from the output themselves.
pub fn build(nvfbc_opt_in: bool, hdr: bool) -> Result<Box<dyn Capture>, CaptureError> {
    set_dpi_awareness();

    if nvfbc_opt_in {
        match NvFbcCapture::new(hdr) {
            Ok(c) => return Ok(Box::new(c)),
            Err(e) => {
                eprintln!("sunburst-capture: NvFBC requested but unavailable ({e}); falling back");
            }
        }
    }

    let first = default_backend(is_win11(), false);
    let second = match first {
        Backend::Wgc => Backend::Dda,
        _ => Backend::Wgc,
    };
    match build_one(first) {
        Ok(c) => Ok(c),
        Err(e1) => build_one(second).map_err(|e2| {
            CaptureError::Backend(format!(
                "both backends failed: {first:?}: {e1}; {second:?}: {e2}"
            ))
        }),
    }
}

/// Construct one specific D3D11 backend. NvFBC is not a default and is not built
/// here.
fn build_one(backend: Backend) -> Result<Box<dyn Capture>, CaptureError> {
    match backend {
        Backend::Wgc => Ok(Box::new(WgcCapture::new()?)),
        Backend::Dda => Ok(Box::new(DdaCapture::new()?)),
        Backend::NvFbc => Err(CaptureError::Backend(
            "NvFBC is not a default backend".into(),
        )),
    }
}

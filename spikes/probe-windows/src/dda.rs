// SPDX-License-Identifier: GPL-2.0-or-later

//! Desktop Duplication, as the control in the experiment.
//!
//! A frames-per-second number from NvFBC means nothing on its own. The claim
//! under test is *comparative* — CLAUDE.md says NvFBC avoids DWM composition and
//! DDA does not — so the useful measurement is both paths on the same machine,
//! against the same desktop, in the same run. That is all this module is for; it
//! is not a capture backend and Phase 3 will not build on it.
//!
//! DDA hands out a new frame only when the desktop presents one, so on an idle
//! desktop the honest metric is **unique frames per second**, not call rate.
//! `AcquireNextFrame` with a zero timeout returns `DXGI_ERROR_WAIT_TIMEOUT`
//! whenever nothing is ready, which is the behaviour being measured rather than
//! an error to report.

use sunburst_core::instr::clock;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter1,
    IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

pub struct Dda {
    pub attempts: u32,
    pub new_frames: u32,
    pub elapsed_ns: u64,
}

impl Dda {
    /// New frames per second — the number comparable to NvFBC's unique rate.
    pub fn fps(&self) -> f64 {
        if self.elapsed_ns == 0 {
            return 0.0;
        }
        f64::from(self.new_frames) * 1e9 / self.elapsed_ns as f64
    }
}

fn duplication() -> Result<IDXGIOutputDuplication, String> {
    // SAFETY: standard DXGI/D3D11 entry points; every returned interface is
    // refcounted and released by `windows`.
    unsafe {
        let factory: IDXGIFactory1 =
            CreateDXGIFactory1().map_err(|e| format!("CreateDXGIFactory1: {e}"))?;
        let adapter: IDXGIAdapter1 = factory
            .EnumAdapters1(0)
            .map_err(|e| format!("EnumAdapters1: {e}"))?;
        let output = adapter
            .EnumOutputs(0)
            .map_err(|e| format!("EnumOutputs: {e}"))?;
        let output1: IDXGIOutput1 = output.cast().map_err(|e| format!("IDXGIOutput1: {e}"))?;

        let mut device: Option<ID3D11Device> = None;
        D3D11CreateDevice(
            &adapter,
            // Must be UNKNOWN when an adapter is passed; anything else is an
            // invalid-argument error rather than a preference.
            D3D_DRIVER_TYPE_UNKNOWN,
            // No software rasteriser module.
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
        .map_err(|e| format!("D3D11CreateDevice: {e}"))?;
        let device = device.ok_or("D3D11CreateDevice returned no device")?;

        output1
            .DuplicateOutput(&device)
            .map_err(|e| format!("DuplicateOutput: {e}"))
    }
}

/// Poll duplication for `window_ns`, counting frames the desktop actually
/// presented.
pub fn run(window_ns: u64) -> Result<Dda, String> {
    let dup = duplication()?;
    let mut result = Dda {
        attempts: 0,
        new_frames: 0,
        elapsed_ns: 0,
    };

    let started = clock::now();
    loop {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: both out parameters are valid and owned here; a zero timeout
        // makes this non-blocking, matching NvFBC's NOWAIT.
        let acquired = unsafe { dup.AcquireNextFrame(0, &mut info, &mut resource) };
        result.attempts += 1;

        match acquired {
            Ok(()) => {
                // LastPresentTime stays zero when only the cursor moved, which
                // is not a new desktop frame and must not be counted as one.
                if info.LastPresentTime != 0 {
                    result.new_frames += 1;
                }
                // SAFETY: a frame was acquired, so exactly one release is owed.
                unsafe { dup.ReleaseFrame() }.ok();
            }
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {}
            Err(e) => return Err(format!("AcquireNextFrame: {e}")),
        }

        let elapsed = clock::ticks_to_ns(clock::now() - started);
        if elapsed >= window_ns {
            result.elapsed_ns = elapsed;
            break;
        }
    }
    Ok(result)
}

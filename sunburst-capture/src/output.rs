// SPDX-License-Identifier: GPL-2.0-or-later

//! Which DXGI output a capture means, and what that output reports.
//!
//! One place for both, so every backend and the session agree:
//!
//! - [`resolve_output`] turns an [`OutputSelect`] into a DXGI output. `Primary`
//!   is the monitor Windows calls primary, found by `HMONITOR`. That is the one
//!   WGC captures, and DDA now uses it too rather than assuming output 0, with
//!   output 0 as the fallback when no output claims the primary monitor.
//! - [`output_info`] reads that output's desktop rectangle and HDR state for a
//!   caller that is not a backend: the session fills `SessionConfig.hdr` from it
//!   after the HDR toggle, and NvFBC, which has no DXGI output of its own, takes
//!   its mastering metadata from it.
//!
//! HDR is detected from the output's colour space. An HDR desktop's output
//! reports HDR10 (PQ + BT.2020, `RGB_FULL_G2084_NONE_P2020`); the scRGB
//! `G10_NONE_P709` space is the composition surface, not the output signal, and
//! checking it once made colour correct on HDR desktops only. Microsoft's
//! D3D12HDR sample checks exactly this value.

use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput6,
};
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint};
use windows::core::Interface;

use crate::{CaptureError, HdrMetadata, OutputSelect};

/// What an output reports about itself.
#[derive(Clone, Copy, Debug)]
pub struct OutputInfo {
    /// The output's rectangle in virtual-desktop coordinates: left, top, right,
    /// bottom, in physical pixels (the process is per-monitor DPI aware).
    pub desktop: [i32; 4],
    /// The output is in HDR mode.
    pub hdr: bool,
    /// Its mastering metadata, when HDR and the modern desc is available.
    pub hdr_metadata: Option<HdrMetadata>,
}

fn backend(e: windows::core::Error) -> CaptureError {
    CaptureError::Backend(e.to_string())
}

/// The DXGI output `select` names, on the first adapter, with that adapter (a
/// backend builds its device on it).
///
/// A fresh factory each call: a cached one keeps reporting the outputs and
/// colour spaces it saw when it was created, which is exactly wrong right after
/// the session toggles HDR.
pub(crate) fn resolve_output(
    select: OutputSelect,
) -> Result<(IDXGIAdapter1, IDXGIOutput), CaptureError> {
    // SAFETY: standard DXGI entry points; every returned interface is refcounted
    // and released by `windows`.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(backend)?;
        let adapter: IDXGIAdapter1 = factory.EnumAdapters1(0).map_err(backend)?;
        let output = match select {
            OutputSelect::Index(n) => adapter.EnumOutputs(n).map_err(backend)?,
            OutputSelect::Primary => {
                let primary = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
                let mut i = 0;
                let mut found = None;
                while let Ok(out) = adapter.EnumOutputs(i) {
                    if out.GetDesc().is_ok_and(|d| d.Monitor == primary) {
                        found = Some(out);
                        break;
                    }
                    i += 1;
                }
                // No output on this adapter drives the primary monitor (it hangs
                // off another GPU, say): output 0, the old behaviour.
                match found {
                    Some(out) => out,
                    None => adapter.EnumOutputs(0).map_err(backend)?,
                }
            }
        };
        Ok((adapter, output))
    }
}

/// HDR state and mastering metadata from an output's modern (`IDXGIOutput6`)
/// desc. SDR when the modern desc is unavailable.
pub(crate) fn output_hdr(output: &IDXGIOutput) -> (bool, Option<HdrMetadata>) {
    let Ok(o6) = output.cast::<IDXGIOutput6>() else {
        return (false, None);
    };
    // SAFETY: `o6` is a live output interface; GetDesc1 fills a plain descriptor.
    let Ok(d1) = (unsafe { o6.GetDesc1() }) else {
        return (false, None);
    };
    let hdr = d1.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
    let meta = hdr.then_some(HdrMetadata {
        red: d1.RedPrimary,
        green: d1.GreenPrimary,
        blue: d1.BluePrimary,
        white: d1.WhitePoint,
        min_luminance: d1.MinLuminance,
        max_luminance: d1.MaxLuminance,
        max_full_frame_luminance: d1.MaxFullFrameLuminance,
    });
    (hdr, meta)
}

/// The rectangle and HDR state of the output `select` names, read now.
pub fn output_info(select: OutputSelect) -> Result<OutputInfo, CaptureError> {
    let (_, output) = resolve_output(select)?;
    // SAFETY: `output` is a live output interface; GetDesc fills a plain struct.
    let d = unsafe { output.GetDesc() }.map_err(backend)?;
    let r = d.DesktopCoordinates;
    let (hdr, hdr_metadata) = output_hdr(&output);
    Ok(OutputInfo {
        desktop: [r.left, r.top, r.right, r.bottom],
        hdr,
        hdr_metadata,
    })
}

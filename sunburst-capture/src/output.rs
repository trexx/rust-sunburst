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
//!
//! An HDR output also reports its SDR white level: the brightness Windows draws
//! SDR content at (the "SDR content brightness" slider), well above the 80 nits
//! of scRGB 1.0. An SDR stream from an HDR desktop normalises by it before
//! tonemapping; taking 80 nits as white blew every light tone out to white.

use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SDR_WHITE_LEVEL, DISPLAYCONFIG_SOURCE_DEVICE_NAME, DisplayConfigGetDeviceInfo,
    GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
};
use windows::Win32::Foundation::{ERROR_SUCCESS, POINT};
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
    /// The nits an HDR desktop draws SDR white at; `None` when SDR or unreadable.
    pub sdr_white_nits: Option<f32>,
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

/// The nits Windows draws SDR white at on `output`, from its `SDRWhiteLevel`
/// (1000 = 80 nits). `None` if the output is not in the active display
/// configuration or the query fails (pre-1709 Windows).
///
/// DXGI and DisplayConfig name the same monitor differently: DXGI by its GDI
/// device name (`\\.\DISPLAY1`), DisplayConfig by a path. The path whose source
/// carries that GDI name is the one, and the white level lives on its target.
pub(crate) fn sdr_white_nits(output: &IDXGIOutput) -> Option<f32> {
    // SAFETY: `output` is a live output interface; GetDesc fills a plain struct.
    let name = unsafe { output.GetDesc() }.ok()?.DeviceName;

    let mut num_paths = 0u32;
    let mut num_modes = 0u32;
    // SAFETY: out-params for the buffer sizes; QDC_ONLY_ACTIVE_PATHS is a valid flag.
    let sized = unsafe {
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut num_paths, &mut num_modes)
    };
    if sized != ERROR_SUCCESS || num_paths == 0 {
        return None;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); num_paths as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); num_modes as usize];
    // SAFETY: buffers are sized to the counts just queried; the counts are
    // updated in place to what was written.
    let queried = unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut num_paths,
            paths.as_mut_ptr(),
            &mut num_modes,
            modes.as_mut_ptr(),
            None,
        )
    };
    if queried != ERROR_SUCCESS {
        return None;
    }

    for path in paths.iter().take(num_paths as usize) {
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            ..Default::default()
        };
        // SAFETY: `source.header` is a correctly-typed and -sized request packet.
        if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } != ERROR_SUCCESS.0 as i32 {
            continue;
        }
        if source.viewGdiDeviceName != name {
            continue;
        }
        let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                size: size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            SDRWhiteLevel: 0,
        };
        // SAFETY: `white.header` is a correctly-typed and -sized request packet.
        if unsafe { DisplayConfigGetDeviceInfo(&mut white.header) } != ERROR_SUCCESS.0 as i32 {
            return None;
        }
        return sdr_white_level_nits(white.SDRWhiteLevel);
    }
    None
}

/// `SDRWhiteLevel` to nits: the value is a multiplier on 80 nits, times 1000.
/// Zero (nothing reported) is `None`.
pub(crate) fn sdr_white_level_nits(level: u32) -> Option<f32> {
    (level > 0).then(|| level as f32 / 1000.0 * 80.0)
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
        sdr_white_nits: if hdr { sdr_white_nits(&output) } else { None },
    })
}

#[cfg(test)]
mod tests {
    use super::sdr_white_level_nits;

    #[test]
    fn sdr_white_level_is_a_multiplier_on_80_nits() {
        assert_eq!(sdr_white_level_nits(1000), Some(80.0));
        assert_eq!(sdr_white_level_nits(2500), Some(200.0));
        assert_eq!(sdr_white_level_nits(0), None, "nothing reported");
    }
}

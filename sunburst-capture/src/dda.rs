// SPDX-License-Identifier: GPL-2.0-or-later

//! Desktop Duplication (DXGI) — the Win10 default backend, and the Win11
//! cross-fallback.
//!
//! Captures the primary monitor into an `ID3D11Texture2D`, yielded as
//! [`Frame::Texture`]. The frame is the duplication's own surface, valid only
//! until the next [`Capture::acquire`] (which calls `ReleaseFrame`) — the convert
//! stage reads it before acquiring again, so no copy is taken here.
//!
//! HDR: when the output reports the scRGB colour space, the duplication is
//! created with `DuplicateOutput1` and an `R16G16B16A16_FLOAT` format list, so
//! frames arrive as scRGB linear FP16 (what the convert shader expects). The
//! spike this is derived from only did 8-bit; this adds the HDR path.
//!
//! `AccessLost` — a mode change, fullscreen transition, or desktop switch — is
//! recoverable and *expected*: [`Capture::acquire`] returns
//! [`CaptureError::AccessLost`] and the owner rebuilds. The secure desktop / DRM
//! surfaces as [`CaptureError::Unavailable`].

use std::time::Duration;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709, DXGI_FORMAT, DXGI_FORMAT_R16G16B16A16_FLOAT,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_DENIED, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutput5,
    IDXGIOutput6, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

use crate::{
    Backend, Caps, Capture, CaptureError, Frame, FrameMeta, HdrMetadata, TextureFormat,
    TextureFrame,
};

/// A live Desktop Duplication of the primary output.
pub struct DdaCapture {
    /// Kept alive: the duplication borrows this device.
    _device: ID3D11Device,
    dup: IDXGIOutputDuplication,
    caps: Caps,
    format: TextureFormat,
    /// Whether a frame is currently checked out and owes a `ReleaseFrame`.
    holding: bool,
}

impl DdaCapture {
    /// Build a duplication of the primary monitor.
    pub fn new() -> Result<DdaCapture, CaptureError> {
        // SAFETY: standard DXGI/D3D11 entry points; every returned interface is
        // refcounted and released by `windows`.
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(backend)?;
            let adapter: IDXGIAdapter1 = factory.EnumAdapters1(0).map_err(backend)?;
            let output = adapter.EnumOutputs(0).map_err(backend)?;

            // Dimensions + HDR (and its mastering metadata) from the modern
            // output desc where available.
            let (width, height, hdr, hdr_metadata) = match output.cast::<IDXGIOutput6>() {
                Ok(o6) => {
                    let d1 = o6.GetDesc1().map_err(backend)?;
                    let r = d1.DesktopCoordinates;
                    let hdr = d1.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709;
                    let meta = hdr.then_some(HdrMetadata {
                        red: d1.RedPrimary,
                        green: d1.GreenPrimary,
                        blue: d1.BluePrimary,
                        white: d1.WhitePoint,
                        min_luminance: d1.MinLuminance,
                        max_luminance: d1.MaxLuminance,
                        max_full_frame_luminance: d1.MaxFullFrameLuminance,
                    });
                    (
                        (r.right - r.left) as u32,
                        (r.bottom - r.top) as u32,
                        hdr,
                        meta,
                    )
                }
                Err(_) => {
                    let d = output.GetDesc().map_err(backend)?;
                    let r = d.DesktopCoordinates;
                    (
                        (r.right - r.left) as u32,
                        (r.bottom - r.top) as u32,
                        false,
                        None,
                    )
                }
            };

            let mut device: Option<ID3D11Device> = None;
            D3D11CreateDevice(
                &adapter,
                // Must be UNKNOWN when an adapter is passed.
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
            .map_err(backend)?;
            let device = device.ok_or_else(|| CaptureError::Backend("no D3D11 device".into()))?;

            // HDR → DuplicateOutput1 with an FP16 format list; SDR → plain
            // DuplicateOutput (BGRA8).
            let (dup, format) = if hdr {
                let output5: IDXGIOutput5 = output.cast().map_err(backend)?;
                let formats: [DXGI_FORMAT; 1] = [DXGI_FORMAT_R16G16B16A16_FLOAT];
                let dup = output5
                    .DuplicateOutput1(&device, 0, &formats)
                    .map_err(backend)?;
                (dup, TextureFormat::Rgba16Float)
            } else {
                let output1: IDXGIOutput1 = output.cast().map_err(backend)?;
                let dup = output1.DuplicateOutput(&device).map_err(backend)?;
                (dup, TextureFormat::Bgra8)
            };

            Ok(DdaCapture {
                _device: device,
                dup,
                caps: Caps {
                    backend: Backend::Dda,
                    hdr,
                    width,
                    height,
                    hdr_metadata,
                },
                format,
                holding: false,
            })
        }
    }

    /// Release the previously handed-out frame, if any. Idempotent.
    fn release(&mut self) {
        if self.holding {
            // SAFETY: exactly one release per successful acquire.
            unsafe {
                let _ = self.dup.ReleaseFrame();
            }
            self.holding = false;
        }
    }
}

impl Capture for DdaCapture {
    fn acquire(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
        // The prior frame's surface is invalid once we ask for the next.
        self.release();

        let ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: both out-params are valid; a live duplication.
        let r = unsafe { self.dup.AcquireNextFrame(ms, &mut info, &mut resource) };
        match r {
            Ok(()) => {
                let resource = resource
                    .ok_or_else(|| CaptureError::Backend("acquire gave no surface".into()))?;
                let texture: ID3D11Texture2D = resource.cast().map_err(backend)?;
                self.holding = true;
                Ok(Some(Frame::Texture(TextureFrame {
                    texture,
                    format: self.format,
                    meta: FrameMeta {
                        width: self.caps.width,
                        height: self.caps.height,
                        hdr: self.caps.hdr,
                        // QPC ticks at present, for glass-to-glass accounting.
                        present_qpc: info.LastPresentTime,
                    },
                })))
            }
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => Ok(None),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => Err(CaptureError::AccessLost),
            // The secure desktop (UAC / lock screen) or DRM-protected content.
            Err(e) if e.code() == DXGI_ERROR_ACCESS_DENIED => Err(CaptureError::Unavailable),
            Err(e) => Err(CaptureError::Backend(format!("AcquireNextFrame: {e}"))),
        }
    }

    fn caps(&self) -> Caps {
        self.caps
    }
}

impl Drop for DdaCapture {
    fn drop(&mut self) {
        self.release();
    }
}

/// Map a `windows` error into a fatal backend error.
fn backend(e: windows::core::Error) -> CaptureError {
    CaptureError::Backend(e.to_string())
}

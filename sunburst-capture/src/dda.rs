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

use std::time::{Duration, Instant};

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_R16G16B16A16_FLOAT};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_DENIED, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

use crate::{Backend, Caps, Capture, CaptureError, Frame, FrameMeta, TextureFormat, TextureFrame};

/// A live Desktop Duplication of the primary output.
pub struct DdaCapture {
    /// Kept alive: the duplication borrows this device.
    _device: ID3D11Device,
    dup: IDXGIOutputDuplication,
    caps: Caps,
    format: TextureFormat,
    /// Whether a frame is currently checked out and owes a `ReleaseFrame`.
    holding: bool,
    /// Whether this duplication has handed out a frame yet. Until it has, even
    /// an update that looks pointer-only is delivered, so a fresh capture on a
    /// still desktop is never left without its first image.
    delivered: bool,
}

impl DdaCapture {
    /// Build a duplication of the selected monitor (`Primary` = the primary
    /// monitor; `Index(n)` = the n-th output, for a virtual display).
    pub fn new(select: crate::OutputSelect) -> Result<DdaCapture, CaptureError> {
        let (adapter, output) = crate::output::resolve_output(select)?;
        // SAFETY: standard DXGI/D3D11 entry points; every returned interface is
        // refcounted and released by `windows`.
        unsafe {
            // Dimensions from the output desc; HDR + mastering from the modern
            // desc via the shared helper (WGC uses the same detection).
            let d = output.GetDesc().map_err(backend)?;
            let r = d.DesktopCoordinates;
            let (width, height) = ((r.right - r.left) as u32, (r.bottom - r.top) as u32);
            let (hdr, hdr_metadata) = crate::output::output_hdr(&output);

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
                delivered: false,
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

        let deadline = Instant::now() + timeout;
        loop {
            // Round up: truncating would wake early and spin on a sub-millisecond
            // remainder.
            let remaining = deadline.saturating_duration_since(Instant::now());
            let ms = remaining
                .as_nanos()
                .div_ceil(1_000_000)
                .min(u32::MAX as u128) as u32;
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            // SAFETY: both out-params are valid; a live duplication.
            let r = unsafe { self.dup.AcquireNextFrame(ms, &mut info, &mut resource) };
            match r {
                Ok(()) => {
                    self.holding = true;
                    // A pointer-only update: DXGI wakes us for a mouse move with
                    // `LastPresentTime == 0` and nothing accumulated — the desktop
                    // image is unchanged. The cursor is drawn client-side and DDA
                    // never composites it, so this is not a frame: encoding it
                    // wastes an encode slot and puts `present_qpc = 0` on the wire.
                    if self.delivered && info.LastPresentTime == 0 && info.AccumulatedFrames == 0 {
                        self.release();
                        if Instant::now() >= deadline {
                            return Ok(None);
                        }
                        continue;
                    }
                    let resource = resource
                        .ok_or_else(|| CaptureError::Backend("acquire gave no surface".into()))?;
                    let texture: ID3D11Texture2D = resource.cast().map_err(backend)?;
                    self.delivered = true;
                    return Ok(Some(Frame::Texture(TextureFrame {
                        texture,
                        format: self.format,
                        meta: FrameMeta {
                            width: self.caps.width,
                            height: self.caps.height,
                            hdr: self.caps.hdr,
                            // QPC ticks at present, for glass-to-glass accounting.
                            present_qpc: info.LastPresentTime,
                        },
                    })));
                }
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                    return Err(CaptureError::AccessLost);
                }
                // The secure desktop (UAC / lock screen) or DRM-protected content.
                Err(e) if e.code() == DXGI_ERROR_ACCESS_DENIED => {
                    return Err(CaptureError::Unavailable);
                }
                Err(e) => return Err(CaptureError::Backend(format!("AcquireNextFrame: {e}"))),
            }
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

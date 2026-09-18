// SPDX-License-Identifier: GPL-2.0-or-later

//! Windows.Graphics.Capture — the Win11 default backend.
//!
//! Captures the primary monitor into an `R16G16B16A16_FLOAT` (scRGB) texture via
//! a **free-threaded** frame pool. CLAUDE.md is explicit that the free-threaded
//! pool is mandatory: the non-free-threaded one dispatches on a UI thread and
//! destroys latency.
//!
//! Like DDA, the yielded texture is the pool surface and is valid only until the
//! next [`Capture::acquire`] — the previous frame is held until then so the pool
//! does not recycle its slot underneath the convert stage.
//!
//! WGC delivers frames by event; this pulls. [`Capture::acquire`] calls
//! `TryGetNextFrame`, which returns `E_POINTER` when nothing is ready — polled
//! within the timeout budget. (A `FrameArrived`-signalled wait is a later
//! refinement; the poll keeps the first cut simple and correct.)

use std::time::{Duration, Instant};

use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{E_POINTER, POINT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET, IDXGIDevice,
};
use windows::Win32::Graphics::Gdi::{HMONITOR, MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{HRESULT, Interface};

use crate::{Backend, Caps, Capture, CaptureError, Frame, FrameMeta, TextureFormat, TextureFrame};

/// A live Windows.Graphics.Capture session on the primary monitor.
pub struct WgcCapture {
    _device: ID3D11Device,
    _d3d: IDirect3DDevice,
    _item: GraphicsCaptureItem,
    _session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    /// The frame handed out last, kept alive so the pool slot the caller is still
    /// reading is not recycled until the next `acquire`.
    current: Option<Direct3D11CaptureFrame>,
    caps: Caps,
}

impl WgcCapture {
    /// Build a capture session on the primary monitor.
    pub fn new() -> Result<WgcCapture, CaptureError> {
        // SAFETY: standard D3D11 + WinRT interop; every returned interface is
        // refcounted and released by `windows`.
        unsafe {
            let mut device: Option<ID3D11Device> = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
            .map_err(backend)?;
            let device = device.ok_or_else(|| CaptureError::Backend("no D3D11 device".into()))?;

            // Wrap the D3D11 device as a WinRT IDirect3DDevice for the frame pool.
            let dxgi: IDXGIDevice = device.cast().map_err(backend)?;
            let inspectable = CreateDirect3D11DeviceFromDXGIDevice(&dxgi).map_err(backend)?;
            let d3d: IDirect3DDevice = inspectable.cast().map_err(backend)?;

            // The primary monitor as a capture item, via the Win32 interop factory.
            let hmon: HMONITOR = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
            let interop: IGraphicsCaptureItemInterop =
                windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
                    .map_err(backend)?;
            let item: GraphicsCaptureItem = interop.CreateForMonitor(hmon).map_err(backend)?;
            let size = item.Size().map_err(backend)?;

            // Free-threaded is mandatory (CLAUDE.md); FP16 scRGB matches the
            // convert shader.
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &d3d,
                DirectXPixelFormat::R16G16B16A16Float,
                2,
                size,
            )
            .map_err(backend)?;
            let session = pool.CreateCaptureSession(&item).map_err(backend)?;
            session.StartCapture().map_err(backend)?;

            Ok(WgcCapture {
                _device: device,
                _d3d: d3d,
                _item: item,
                _session: session,
                pool,
                current: None,
                caps: Caps {
                    backend: Backend::Wgc,
                    // The FP16 pool carries scRGB; whether the output is true HDR
                    // (PQ) + its mastering metadata is a DXGI-output query shared
                    // with DDA — a refinement.
                    hdr: false,
                    width: size.Width.max(0) as u32,
                    height: size.Height.max(0) as u32,
                    hdr_metadata: None,
                },
            })
        }
    }
}

impl Capture for WgcCapture {
    fn acquire(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
        // Release the previous frame's pool slot before pulling the next.
        self.current = None;

        let deadline = Instant::now() + timeout;
        loop {
            match self.pool.TryGetNextFrame() {
                Ok(frame) => {
                    let surface = frame.Surface().map_err(backend)?;
                    let access: IDirect3DDxgiInterfaceAccess = surface.cast().map_err(backend)?;
                    // SAFETY: the surface wraps a live D3D11 texture; GetInterface
                    // AddRefs it, so the texture outlives `frame`'s slot return.
                    let texture: ID3D11Texture2D =
                        unsafe { access.GetInterface() }.map_err(backend)?;
                    let present = frame.SystemRelativeTime().map(|t| t.Duration).unwrap_or(0);
                    self.current = Some(frame);
                    return Ok(Some(Frame::Texture(TextureFrame {
                        texture,
                        format: TextureFormat::Rgba16Float,
                        meta: FrameMeta {
                            width: self.caps.width,
                            height: self.caps.height,
                            hdr: self.caps.hdr,
                            present_qpc: present,
                        },
                    })));
                }
                // No frame ready yet — poll within the timeout budget.
                Err(e) if e.code() == E_POINTER => {
                    if Instant::now() >= deadline {
                        return Ok(None);
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) if is_device_lost(e.code()) => return Err(CaptureError::AccessLost),
                Err(e) => return Err(CaptureError::Backend(format!("TryGetNextFrame: {e}"))),
            }
        }
    }

    fn caps(&self) -> Caps {
        self.caps
    }
}

/// Device-removed / reset are recoverable by rebuilding the whole session.
fn is_device_lost(code: HRESULT) -> bool {
    code == DXGI_ERROR_DEVICE_REMOVED || code == DXGI_ERROR_DEVICE_RESET
}

/// Map a `windows` error into a fatal backend error.
fn backend(e: windows::core::Error) -> CaptureError {
    CaptureError::Backend(e.to_string())
}

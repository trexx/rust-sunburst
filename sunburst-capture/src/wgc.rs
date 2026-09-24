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
//! WGC delivers frames by event. A `FrameArrived` handler signals a Win32
//! auto-reset event, and [`Capture::acquire`] waits on it rather than
//! sleep-polling `TryGetNextFrame` — the wait wakes the instant a frame lands,
//! where a 1 ms poll adds up to a millisecond of avoidable latency.

use std::time::{Duration, Instant};

use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::E_POINTER;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET, IDXGIDevice,
};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{HRESULT, IInspectable, Interface};

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
    /// Auto-reset event the `FrameArrived` handler signals, so `acquire` blocks
    /// until a frame is ready instead of polling.
    frame_ready: HANDLE,
    caps: Caps,
}

impl WgcCapture {
    /// Build a capture session on the selected monitor (`Primary` = the primary
    /// monitor; `Index(n)` = the n-th DXGI output, for a virtual display).
    pub fn new(output: crate::OutputSelect) -> Result<WgcCapture, CaptureError> {
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

            // The monitor to capture: the same DXGI output DDA would duplicate
            // (the primary, or the n-th output for a selected virtual display),
            // as an HMONITOR. Its HDR state comes from the same query DDA uses:
            // WGC's FP16 pool carries SDR and HDR alike, so this is what
            // distinguishes them.
            let (_, dxgi_output) = crate::output::resolve_output(output)?;
            let hmon: HMONITOR = dxgi_output.GetDesc().map_err(backend)?.Monitor;
            let (hdr, hdr_metadata) = crate::output::output_hdr(&dxgi_output);
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
            // Auto-reset, initially unsignalled: each successful wait consumes one
            // arrival, and any missed signal is caught by the timeout in `acquire`.
            let frame_ready = CreateEventW(None, false, false, None).map_err(backend)?;
            // Signal the event from the frame-arrived callback. The handler is
            // `'static`, so it carries the handle as a raw `isize` (a `HANDLE` is
            // not `Send`); the pool holds the registration until it is dropped.
            let raw = frame_ready.0 as isize;
            let handler = TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(
                move |_pool, _args| {
                    // `raw` is the live auto-reset event (already inside `new`'s
                    // unsafe block); SetEvent only signals it, and the handler
                    // lives no longer than the pool that holds it.
                    let _ = SetEvent(HANDLE(raw as *mut core::ffi::c_void));
                    Ok(())
                },
            );
            pool.FrameArrived(&handler).map_err(backend)?;

            let session = pool.CreateCaptureSession(&item).map_err(backend)?;
            // The client draws the pointer itself from separately-delivered shape
            // data (`sunburst-server`'s cursor poller). Left enabled, WGC bakes the
            // cursor into every frame — two cursors on the TV — and a mouse move
            // alone makes DWM compose a frame, so an idle desktop keeps encoding.
            // Needs build 19041 (IGraphicsCaptureSession2); older builds refuse,
            // and WGC is only the fallback there.
            let _ = session.SetIsCursorCaptureEnabled(false);
            session.StartCapture().map_err(backend)?;

            Ok(WgcCapture {
                _device: device,
                _d3d: d3d,
                _item: item,
                _session: session,
                pool,
                current: None,
                frame_ready,
                caps: Caps {
                    backend: Backend::Wgc,
                    // The FP16 pool carries scRGB for both SDR and HDR desktops;
                    // `hdr` (from the DXGI output query above) is what says which,
                    // so the convert stage picks BT.709 vs BT.2020 PQ correctly.
                    hdr,
                    width: size.Width.max(0) as u32,
                    height: size.Height.max(0) as u32,
                    hdr_metadata,
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
                Ok(mut frame) => {
                    // The pool hands frames out oldest first. After a long encode
                    // it can hold a newer one than this; take the newest, and let
                    // the older go back to the pool, so what gets encoded is the
                    // latest desktop rather than one a frame behind.
                    while let Ok(newer) = self.pool.TryGetNextFrame() {
                        let _ = frame.Close();
                        frame = newer;
                    }
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
                // No frame ready yet — wait on the arrival event within the
                // remaining budget rather than spinning.
                Err(e) if e.code() == E_POINTER => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Ok(None);
                    }
                    // Round up: truncating would wake early and spin on a
                    // sub-millisecond remainder.
                    let ms = remaining
                        .as_nanos()
                        .div_ceil(1_000_000)
                        .min(u32::MAX as u128) as u32;
                    // SAFETY: `frame_ready` is our live auto-reset event.
                    let waited = unsafe { WaitForSingleObject(self.frame_ready, ms) };
                    if waited != WAIT_OBJECT_0 {
                        return Ok(None); // timed out; the caller reuses the last frame
                    }
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

impl Drop for WgcCapture {
    fn drop(&mut self) {
        // SAFETY: our event handle, closed once; the pool (dropped with `self`)
        // unregisters the handler that referenced it.
        unsafe {
            let _ = CloseHandle(self.frame_ready);
        }
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

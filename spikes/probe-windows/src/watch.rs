// SPDX-License-Identifier: GPL-2.0-or-later

//! One interface over the three capture backends, for the latency harness.
//!
//! The harness does not care how a frame was obtained, only whether one arrived
//! and what the bytes at the read point are. Keeping that behind a trait is what
//! lets the same measurement, the same percentile code and the same verdict apply
//! to all three — which is the only way the numbers are comparable.
//!
//! Each backend is run **on its own**, never concurrently. Three capture sessions
//! against one desktop would measure contention between them.

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter1,
    IDXGIDevice, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

use crate::readback::{ReadPoint, SAMPLE_BYTES};

/// A capture backend the harness can poll.
pub trait Watcher {
    fn name(&self) -> &'static str;
    /// Poll once. `Ok(true)` means a frame arrived and `out` was filled;
    /// `Ok(false)` means nothing was ready, which is not an error.
    fn poll(&mut self, out: &mut [u8; SAMPLE_BYTES]) -> Result<bool, String>;
}

fn create_device(
    adapter: Option<&IDXGIAdapter1>,
) -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    // SAFETY: standard D3D11 setup; both out parameters are valid. The driver
    // type must be UNKNOWN when an adapter is supplied and HARDWARE when not.
    unsafe {
        D3D11CreateDevice(
            adapter
                .map(|a| a.cast::<windows::Win32::Graphics::Dxgi::IDXGIAdapter>())
                .transpose()
                .map_err(|e| format!("IDXGIAdapter: {e}"))?
                .as_ref(),
            if adapter.is_some() {
                D3D_DRIVER_TYPE_UNKNOWN
            } else {
                D3D_DRIVER_TYPE_HARDWARE
            },
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .map_err(|e| format!("D3D11CreateDevice: {e}"))?;
    Ok((
        device.ok_or("D3D11CreateDevice returned no device")?,
        context.ok_or("D3D11CreateDevice returned no context")?,
    ))
}

// ---------------------------------------------------------------- DDA

pub struct Dda {
    dup: IDXGIOutputDuplication,
    read: ReadPoint,
    x: u32,
    y: u32,
}

impl Dda {
    pub fn open(x: u32, y: u32) -> Result<Dda, String> {
        // SAFETY: standard DXGI enumeration; interfaces are refcounted.
        let (adapter, output1) = unsafe {
            let factory: IDXGIFactory1 =
                CreateDXGIFactory1().map_err(|e| format!("CreateDXGIFactory1: {e}"))?;
            let adapter: IDXGIAdapter1 = factory
                .EnumAdapters1(0)
                .map_err(|e| format!("EnumAdapters1: {e}"))?;
            let output = adapter
                .EnumOutputs(0)
                .map_err(|e| format!("EnumOutputs: {e}"))?;
            let output1: IDXGIOutput1 = output.cast().map_err(|e| format!("IDXGIOutput1: {e}"))?;
            (adapter, output1)
        };
        let (device, context) = create_device(Some(&adapter))?;
        // SAFETY: `device` is live and belongs to the same adapter as `output1`.
        let dup = unsafe { output1.DuplicateOutput(&device) }
            .map_err(|e| format!("DuplicateOutput: {e}"))?;
        Ok(Dda {
            dup,
            read: ReadPoint::new(device, context),
            x,
            y,
        })
    }
}

impl Watcher for Dda {
    fn name(&self) -> &'static str {
        "DDA   "
    }

    fn poll(&mut self, out: &mut [u8; SAMPLE_BYTES]) -> Result<bool, String> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: both out parameters are valid; a zero timeout makes this
        // non-blocking.
        let acquired = unsafe { self.dup.AcquireNextFrame(0, &mut info, &mut resource) };
        match acquired {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
            Err(e) => return Err(format!("AcquireNextFrame: {e}")),
        }

        let sampled = resource
            .ok_or_else(|| "AcquireNextFrame returned no resource".to_string())
            .and_then(|r| {
                r.cast::<ID3D11Texture2D>()
                    .map_err(|e| format!("texture: {e}"))
            })
            .and_then(|texture| self.read.sample(&texture, self.x, self.y, out));

        // Owed exactly one release per successful acquire, whatever the sample
        // did.
        // SAFETY: a frame was acquired above.
        unsafe { self.dup.ReleaseFrame() }.ok();
        sampled.map(|()| true)
    }
}

// ---------------------------------------------------------------- WGC

pub struct Wgc {
    pool: Direct3D11CaptureFramePool,
    /// Held so the session stays running; dropping it stops capture.
    _session: windows::Graphics::Capture::GraphicsCaptureSession,
    read: ReadPoint,
    x: u32,
    y: u32,
}

impl Wgc {
    pub fn open(x: u32, y: u32) -> Result<Wgc, String> {
        let (device, context) = create_device(None)?;
        let dxgi: IDXGIDevice = device.cast().map_err(|e| format!("IDXGIDevice: {e}"))?;

        // WinRT wants its own device handle around the same D3D11 device.
        // SAFETY: `dxgi` is a live IDXGIDevice.
        let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .map_err(|e| format!("CreateDirect3D11DeviceFromDXGIDevice: {e}"))?;
        let winrt_device: windows::Graphics::DirectX::Direct3D11::IDirect3DDevice = inspectable
            .cast()
            .map_err(|e| format!("IDirect3DDevice: {e}"))?;

        // The monitor the presenter's window is on. Capture is per-monitor, and
        // an item for the wrong one would simply never show the signal.
        // SAFETY: MonitorFromPoint takes a POINT by value and returns a handle
        // the caller does not own; DEFAULTTOPRIMARY means it cannot fail.
        let monitor = unsafe {
            MonitorFromPoint(
                windows::Win32::Foundation::POINT {
                    x: x as i32,
                    y: y as i32,
                },
                MONITOR_DEFAULTTOPRIMARY,
            )
        };
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .map_err(|e| format!("IGraphicsCaptureItemInterop: {e}"))?;
        // SAFETY: `monitor` is a live HMONITOR from MonitorFromPoint.
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(monitor) }
            .map_err(|e| format!("CreateForMonitor: {e}"))?;

        // Float16 rather than BGRA8: it is what CLAUDE.md specifies for HDR, so
        // measuring it measures the configuration Phase 3 will actually ship.
        // CreateFreeThreaded is not optional -- the non-free-threaded pool
        // dispatches on a UI thread and destroys the latency being measured.
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::R16G16B16A16Float,
            2,
            item.Size().map_err(|e| format!("item.Size: {e}"))?,
        )
        .map_err(|e| format!("CreateFreeThreaded: {e}"))?;

        let session = pool
            .CreateCaptureSession(&item)
            .map_err(|e| format!("CreateCaptureSession: {e}"))?;
        session
            .StartCapture()
            .map_err(|e| format!("StartCapture: {e}"))?;

        Ok(Wgc {
            pool,
            _session: session,
            read: ReadPoint::new(device, context),
            x,
            y,
        })
    }
}

impl Watcher for Wgc {
    fn name(&self) -> &'static str {
        "WGC   "
    }

    fn poll(&mut self, out: &mut [u8; SAMPLE_BYTES]) -> Result<bool, String> {
        let Ok(frame) = self.pool.TryGetNextFrame() else {
            return Ok(false);
        };
        let surface = frame.Surface().map_err(|e| format!("frame.Surface: {e}"))?;
        let access: IDirect3DDxgiInterfaceAccess = surface
            .cast()
            .map_err(|e| format!("IDirect3DDxgiInterfaceAccess: {e}"))?;
        // SAFETY: the surface is a live WinRT Direct3D surface, and the
        // interface requested is the D3D11 texture behind it.
        let texture: ID3D11Texture2D =
            unsafe { access.GetInterface() }.map_err(|e| format!("GetInterface: {e}"))?;

        let sampled = self.read.sample(&texture, self.x, self.y, out);
        // Frames come from a pool of two; not closing one stalls the pool.
        frame.Close().ok();
        sampled.map(|()| true)
    }
}

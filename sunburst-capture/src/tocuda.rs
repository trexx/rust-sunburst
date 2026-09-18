// SPDX-License-Identifier: GPL-2.0-or-later

//! `NvFBCToCuda` — capture that stays on the GPU, promoted to a [`Capture`]
//! backend.
//!
//! NvFBC grabs the desktop into a CUDA device pointer, which this yields as
//! [`Frame::Cuda`] — the frame never crosses an API boundary on its way to
//! NVENC's `CUDADEVICEPTR` input, which is the whole reason this backend exists
//! (see `memory/capture-path-nvfbc-cuda-native`). Opt-in resilience only.
//!
//! The load-bearing subtleties are preserved verbatim from the Phase-0 spike,
//! each of which was wrong at least once: **`Setup` is vtable slot 1** (slot 0 is
//! `GetMaxBufferSize`), the destination buffer is ours to allocate from what it
//! reports, NvFBC **creates its own CUDA context** (neither `pDevice` nor
//! `cudaCtx` is passed) which we adopt via `cuCtxPopCurrent`→`Push`, and at
//! teardown the buffer is freed **before** `Release` while the context is **never
//! destroyed** — it is NvFBC's, and destroying it took the process out on the
//! first run of the probe.

use std::ffi::c_void;
use std::time::Duration;

use crate::cuda::{CUDA_SUCCESS, CuContext, CuDevicePtr, Cuda};
use crate::nvfbc::{self, FrameGrabInfo, result_name};
use crate::{Backend, Caps, Capture, CaptureError, CudaFormat, CudaFrame, Frame, FrameMeta};

/// `NVFBC_SHARED_CUDA` at `NVFBC_DLL_VERSION 0x70`.
const NVFBC_SHARED_CUDA: u32 = 0x1007;

/// `NVFBCToCUDABufferFormat`.
const NVFBC_TOCUDA_ARGB: u32 = 0;
/// A2B10G10R10 — a 10-bit *integer* format, not scRGB FP16.
const NVFBC_TOCUDA_ARGB10: u32 = 1;

const NVFBC_TOCUDA_NOWAIT: u32 = 0x1;
const NVFBC_TOCUDA_NOFLAGS: u32 = 0x0;

/// `bHDRRequest`, bit 1 of the setup bitfield.
const SETUP_FLAG_HDR_REQUEST: u32 = 1 << 1;

/// Vtable slots of `INvFBCCuda`, in declaration order.
#[repr(usize)]
enum Slot {
    GetMaxBufferSize = 0,
    Setup = 1,
    GrabFrame = 2,
    Release = 5,
}

/// `NVFBC_CUDA_SETUP_PARAMS_V1`.
#[repr(C)]
struct SetupParams {
    version: u32,
    flags: u32,
    cursor_capture_event: *mut c_void,
    format: u32,
    reserved: [u32; 61],
    reserved_ptrs: [*const c_void; 31],
}

const SETUP_SIZE: usize = size_of::<SetupParams>();
const _: () = assert!(SETUP_SIZE == 512);
const _: () = assert!(std::mem::offset_of!(SetupParams, cursor_capture_event) == 8);
const _: () = assert!(std::mem::offset_of!(SetupParams, format) == 16);

impl Default for SetupParams {
    fn default() -> Self {
        SetupParams {
            version: 0,
            flags: 0,
            cursor_capture_event: std::ptr::null_mut(),
            format: 0,
            reserved: [0; 61],
            reserved_ptrs: [std::ptr::null(); 31],
        }
    }
}

/// `NVFBC_CUDA_GRAB_FRAME_PARAMS_V1`.
#[repr(C)]
struct GrabParams {
    version: u32,
    flags: u32,
    cuda_device_buffer: CuDevicePtr,
    grab_info: *mut FrameGrabInfo,
    wait_time: u32,
    reserved: [u32; 61],
    reserved_ptrs: [*const c_void; 30],
}

const GRAB_SIZE: usize = size_of::<GrabParams>();
const _: () = assert!(GRAB_SIZE == 512);
const _: () = assert!(std::mem::offset_of!(GrabParams, cuda_device_buffer) == 8);
const _: () = assert!(std::mem::offset_of!(GrabParams, grab_info) == 16);
const _: () = assert!(std::mem::offset_of!(GrabParams, wait_time) == 24);

impl Default for GrabParams {
    fn default() -> Self {
        GrabParams {
            version: 0,
            flags: 0,
            cuda_device_buffer: 0,
            grab_info: std::ptr::null_mut(),
            wait_time: 0,
            reserved: [0; 61],
            reserved_ptrs: [std::ptr::null(); 30],
        }
    }
}

type PfnGetMaxBufferSize = unsafe extern "system" fn(*mut c_void, *mut u32) -> i32;
type PfnSetup = unsafe extern "system" fn(*mut c_void, *mut SetupParams) -> i32;
type PfnGrabFrame = unsafe extern "system" fn(*mut c_void, *mut GrabParams) -> i32;
type PfnRelease = unsafe extern "system" fn(*mut c_void) -> i32;

/// A live `NvFBCToCuda` capture. Owns its CUDA loader and grab buffer.
pub struct NvFbcCapture {
    object: *mut c_void,
    cuda: Cuda,
    /// NvFBC's context, adopted rather than owned — the encode side runs its
    /// convert kernel + NVENC-CUDA session in this same context (see [`context`]).
    context: CuContext,
    buffer: CuDevicePtr,
    format: CudaFormat,
    caps: Caps,
}

impl NvFbcCapture {
    /// Open a keyed session, adopt NvFBC's CUDA context, allocate the destination
    /// buffer, and set up the requested format (ARGB10 for HDR, ARGB8 otherwise).
    pub fn new(hdr: bool) -> Result<NvFbcCapture, CaptureError> {
        let cuda = Cuda::load().map_err(CaptureError::Backend)?;
        if cuda.init() != CUDA_SUCCESS {
            return Err(CaptureError::Backend("cuInit failed".into()));
        }

        let created = nvfbc::create_interface(NVFBC_SHARED_CUDA);
        if !created.succeeded {
            return Err(CaptureError::Backend(format!(
                "NvFBC_CreateEx: {}",
                result_name(created.result)
            )));
        }

        // NvFBC made its own context (no pDevice/cudaCtx passed); take it off the
        // stack and make it ours for the allocation below.
        let context = cuda
            .ctx_pop()
            .map_err(|s| CaptureError::Backend(format!("cuCtxPopCurrent: {s}")))?;
        if cuda.ctx_push(context) != CUDA_SUCCESS {
            return Err(CaptureError::Backend("cuCtxPushCurrent failed".into()));
        }

        let mut cap = NvFbcCapture {
            object: created.object,
            cuda,
            context,
            buffer: 0,
            format: if hdr {
                CudaFormat::Argb10
            } else {
                CudaFormat::Argb8
            },
            caps: Caps {
                backend: Backend::NvFbc,
                hdr,
                width: 0,
                height: 0,
                hdr_metadata: None,
            },
        };

        let bytes = cap
            .max_buffer_size()
            .map_err(|s| CaptureError::Backend(format!("GetMaxBufferSize: {}", result_name(s))))?
            as usize;
        cap.buffer = cap
            .cuda
            .mem_alloc(bytes)
            .map_err(|s| CaptureError::Backend(format!("cuMemAlloc({bytes}): {s}")))?;

        let status = cap.setup(hdr);
        if status != 0 {
            return Err(CaptureError::Backend(format!(
                "NvFBCCudaSetup: {}",
                result_name(status)
            )));
        }
        Ok(cap)
    }

    /// NvFBC's CUDA context, for the encode side to adopt (`cuCtxPushCurrent`) so
    /// its convert kernel and NVENC-CUDA session run where the grab buffer lives.
    pub fn context(&self) -> CuContext {
        self.context
    }

    /// # Safety
    ///
    /// `T` must match the declared signature of `slot` in `INvFBCCuda`.
    unsafe fn slot<T: Copy>(&self, slot: Slot) -> T {
        // SAFETY: the object's first word is its vtable pointer; the caller
        // guarantees the signature.
        unsafe {
            let vtable = *(self.object as *const *const *const c_void);
            nvfbc::cast_fn(*vtable.add(slot as usize))
        }
    }

    fn max_buffer_size(&self) -> Result<u32, i32> {
        let mut bytes = 0u32;
        // SAFETY: slot 0 is NvFBCCudaGetMaxBufferSize(NvU32*).
        let f: PfnGetMaxBufferSize = unsafe { self.slot(Slot::GetMaxBufferSize) };
        // SAFETY: `bytes` is a valid out pointer.
        let status = unsafe { f(self.object, &mut bytes) };
        if status == 0 { Ok(bytes) } else { Err(status) }
    }

    fn setup(&mut self, hdr: bool) -> i32 {
        let mut params = SetupParams {
            version: nvfbc::struct_version(SETUP_SIZE, 1),
            format: if hdr {
                NVFBC_TOCUDA_ARGB10
            } else {
                NVFBC_TOCUDA_ARGB
            },
            ..Default::default()
        };
        if hdr {
            params.flags |= SETUP_FLAG_HDR_REQUEST;
        }
        // SAFETY: slot 1 is NvFBCCudaSetup(NVFBC_CUDA_SETUP_PARAMS*).
        let f: PfnSetup = unsafe { self.slot(Slot::Setup) };
        // SAFETY: `params` is correctly laid out and outlives the call.
        unsafe { f(self.object, &mut params) }
    }

    fn grab(&self, info: &mut FrameGrabInfo, blocking: bool) -> i32 {
        let mut params = GrabParams {
            version: nvfbc::struct_version(GRAB_SIZE, 1),
            flags: if blocking {
                NVFBC_TOCUDA_NOFLAGS
            } else {
                NVFBC_TOCUDA_NOWAIT
            },
            cuda_device_buffer: self.buffer,
            grab_info: info,
            ..Default::default()
        };
        // SAFETY: slot 2 is NvFBCCudaGrabFrame(NVFBC_CUDA_GRAB_FRAME_PARAMS*).
        let f: PfnGrabFrame = unsafe { self.slot(Slot::GrabFrame) };
        // SAFETY: both structs are correctly laid out, and `self.buffer` is a
        // live device allocation of at least the size NvFBC asked for.
        unsafe { f(self.object, &mut params) }
    }
}

impl Capture for NvFbcCapture {
    fn acquire(&mut self, _timeout: Duration) -> Result<Option<Frame>, CaptureError> {
        // NvFBC blocks until the next frame, so the timeout is advisory: a blocking
        // grab always returns a frame (or an error), never `Ok(None)`.
        let mut info = FrameGrabInfo::default();
        let status = self.grab(&mut info, true);
        match status {
            0 => {}
            -3 => return Err(CaptureError::AccessLost), // INVALIDATED_SESSION
            -4 => return Err(CaptureError::Unavailable), // PROTECTED_CONTENT
            other => return Err(CaptureError::Backend(result_name(other).into())),
        }
        if !info.driver_ok() || info.buffer_width == 0 {
            return Ok(None);
        }

        // ARGB and ARGB10 are both 32bpp, so the pitch is the padded width × 4.
        let pitch = info.buffer_width as usize * 4;
        self.caps.width = info.width;
        self.caps.height = info.height;
        Ok(Some(Frame::Cuda(CudaFrame {
            device_ptr: self.buffer,
            pitch,
            format: self.format,
            meta: FrameMeta {
                width: info.width,
                height: info.height,
                hdr: info.is_hdr(),
                // NvFBC's grab info carries no present timestamp.
                present_qpc: 0,
            },
        })))
    }

    fn caps(&self) -> Caps {
        self.caps
    }
}

impl Drop for NvFbcCapture {
    /// Teardown order is not a style choice: free the buffer **before** releasing
    /// the session (the SDK sample does, and its comment says why), and **never**
    /// destroy the context — NvFBC created it.
    fn drop(&mut self) {
        if self.buffer != 0 {
            self.cuda.mem_free(self.buffer);
            self.buffer = 0;
        }
        if !self.object.is_null() {
            // SAFETY: slot 5 is NvFBCCudaRelease(), taking only the object.
            let f: PfnRelease = unsafe { self.slot(Slot::Release) };
            // SAFETY: live object, released exactly once.
            unsafe { f(self.object) };
            self.object = std::ptr::null_mut();
        }
        // self.context is deliberately left alone; it is NvFBC's.
    }
}

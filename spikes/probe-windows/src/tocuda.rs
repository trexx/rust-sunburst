// SPDX-License-Identifier: GPL-2.0-or-later

//! `NvFBCToCuda` — capture that never leaves the GPU.
//!
//! # Why this interface
//!
//! Every earlier measurement went through `NVFBC_TO_SYS`, which copies the whole
//! frame to system memory: ~3.6ms per grab at 4K, paid whether or not the frame
//! is new, and the pixels then have to go *back* to the GPU for colour
//! conversion and NVENC. That shape is wrong for a frame path regardless of how
//! fast it polls, so the number it produced never answered the real question.
//!
//! ToCuda hands back a CUDA device pointer instead. Of the two GPU-resident
//! interfaces it is the better one to measure, because NVENC accepts CUDA device
//! pointers directly — a real pipeline could run capture → convert → encode
//! without crossing an API boundary. `NvFBCToDx9Vid` also gained ARGB10 at 0x70
//! but needs a D3D9Ex device and a shared-surface handoff.
//!
//! # Setup differences from ToSys, all of which bite
//!
//! - The interface id is **0x1007** here. The older header says `0x1006`, and
//!   the ToDx9Vid id moved too, so these are not stable across SDK versions.
//! - **`Setup` is vtable slot 1, not 0** — slot 0 is `GetMaxBufferSize`, and the
//!   destination buffer is ours to allocate from what it reports.
//! - `bHDRRequest` is **bit 1**, and unlike the ToSys one this is read from the
//!   header rather than inferred.
//! - **No D3D9 is needed.** Passing neither `pDevice` nor `cudaCtx` makes NvFBC
//!   create its own CUDA context, which the caller adopts with
//!   `cuCtxPopCurrent` → `cuCtxPushCurrent`. The SDK's own sample gates the D3D9
//!   path behind a flag; this takes the other branch.

use std::ffi::c_void;

use sunburst_core::instr::clock;

use crate::capture::{Capture, FrameGrabInfo, sample_hash};
use crate::cuda::{CUDA_SUCCESS, Cuda, CuContext, CuDevicePtr};
use crate::nvfbc::{self, result_name};

/// `NVFBC_SHARED_CUDA` at `NVFBC_DLL_VERSION 0x70`.
pub const NVFBC_SHARED_CUDA: u32 = 0x1007;

/// `NVFBCToCUDABufferFormat`.
const NVFBC_TOCUDA_ARGB: u32 = 0;
/// A2B10G10R10 — a 10-bit *integer* format, not scRGB FP16.
const NVFBC_TOCUDA_ARGB10: u32 = 1;

/// `NVFBC_TOCUDA_NOWAIT` / `NVFBC_TOCUDA_NOFLAGS`.
const NVFBC_TOCUDA_NOWAIT: u32 = 0x1;
const NVFBC_TOCUDA_NOFLAGS: u32 = 0x0;

/// `bHDRRequest`, bit 1 of the setup bitfield, after `bEnableSeparateCursorCapture`.
const SETUP_FLAG_HDR_REQUEST: u32 = 1 << 1;

/// Vtable slots of `INvFBCCuda_v3`, in declaration order.
#[repr(usize)]
enum Slot {
    GetMaxBufferSize = 0,
    Setup = 1,
    GrabFrame = 2,
    #[expect(dead_code, reason = "documents the vtable; slot order is the point")]
    GpuBasedCpuSleep = 3,
    #[expect(dead_code, reason = "documents the vtable; slot order is the point")]
    CursorCapture = 4,
    Release = 5,
}

/// `NVFBC_CUDA_SETUP_PARAMS_V1`.
#[repr(C)]
struct SetupParams {
    version: u32,
    /// `bEnableSeparateCursorCapture:1`, `bHDRRequest:1`, then 30 reserved.
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

/// A live `INvFBCCuda_v3` with its destination buffer.
pub struct ToCuda<'a> {
    object: *mut c_void,
    cuda: &'a Cuda,
    /// NvFBC's context, adopted rather than owned — recorded so it is obvious
    /// at the drop site that this is not ours to destroy.
    #[expect(dead_code, reason = "documents ownership; see Drop")]
    context: CuContext,
    buffer: CuDevicePtr,
}

/// Bytes pulled back per grab to tell one frame from another.
///
/// ~10µs against a grab that should be well under a millisecond, and it runs
/// outside the timed region. Copying the whole 33MB frame would reintroduce
/// exactly the sysmem transfer this interface exists to avoid.
const SAMPLE_BYTES: usize = 4096;

impl<'a> ToCuda<'a> {
    /// # Safety
    ///
    /// `object` must be a live `INvFBCCuda` from `NvFBC_CreateEx`, not owned
    /// elsewhere.
    unsafe fn new(object: *mut c_void, cuda: &'a Cuda, context: CuContext) -> ToCuda<'a> {
        ToCuda {
            object,
            cuda,
            context,
            buffer: 0,
        }
    }

    /// # Safety
    ///
    /// `T` must match the declared signature of that slot in `INvFBCCuda_v3`.
    unsafe fn slot<T: Copy>(&self, slot: Slot) -> T {
        // SAFETY: the object's first word is its vtable pointer, and the caller
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

    fn setup(&mut self, ten_bit: bool, hdr: bool) -> i32 {
        let mut params = SetupParams {
            version: nvfbc::struct_version(SETUP_SIZE, 1),
            format: if ten_bit {
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

impl Drop for ToCuda<'_> {
    /// Teardown order is not a style choice.
    ///
    /// The SDK's own sample frees the device buffer **before** releasing the
    /// session, and its comment says so explicitly. Getting it backwards frees
    /// an allocation in a context the session may already have torn down.
    ///
    /// And the context is **not destroyed here at all**. NvFBC created it,
    /// because neither `pDevice` nor `cudaCtx` was passed to `CreateEx`; the
    /// sample only calls `cuCtxDestroy` on the D3D9 branch, where the caller
    /// made the context itself. Destroying someone else's is what killed the
    /// first run of this probe — it took the process out after the blocking
    /// grabs, before the DDA control could run.
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

/// Open a keyed ToCuda session, adopt NvFBC's CUDA context, and allocate the
/// destination buffer.
pub fn open(cuda: &Cuda, ten_bit: bool, hdr: bool) -> Option<ToCuda<'_>> {
    let created = nvfbc::create_interface(NVFBC_SHARED_CUDA);
    if !created.succeeded {
        println!(
            "    no ToCuda session: CreateEx said {}",
            result_name(created.result)
        );
        return None;
    }

    // NvFBC made its own context because neither pDevice nor cudaCtx was
    // passed; take it off the stack and make it ours for the allocations below.
    let context = match cuda.ctx_pop() {
        Ok(ctx) => ctx,
        Err(status) => {
            println!("    cuCtxPopCurrent failed: {status}");
            return None;
        }
    };
    if cuda.ctx_push(context) != CUDA_SUCCESS {
        println!("    cuCtxPushCurrent failed");
        return None;
    }

    // SAFETY: a fresh live object from NvFBC_CreateEx, wrapped once.
    let mut session = unsafe { ToCuda::new(created.object, cuda, context) };

    let bytes = match session.max_buffer_size() {
        Ok(bytes) => bytes as usize,
        Err(status) => {
            println!("    GetMaxBufferSize failed: {}", result_name(status));
            return None;
        }
    };
    println!("    GetMaxBufferSize: {bytes} bytes");

    match cuda.mem_alloc(bytes) {
        Ok(ptr) => session.buffer = ptr,
        Err(status) => {
            println!("    cuMemAlloc({bytes}) failed: {status}");
            return None;
        }
    }

    let status = session.setup(ten_bit, hdr);
    if status != 0 {
        println!("    NvFBCCudaSetup failed: {}", result_name(status));
        return None;
    }
    Some(session)
}

/// Grab `count` frames, timing each.
pub fn run(session: &mut ToCuda, count: u32, blocking: bool) -> Capture {
    let mut result = Capture {
        blocking,
        overhead_ns: 0,
        setup_result: 0,
        grabs: 0,
        failures: 0,
        unique: 0,
        elapsed_ns: 0,
        p50_ns: 0,
        p99_ns: 0,
        width: 0,
        height: 0,
        is_hdr: false,
        blocking_grabs: 0,
        driver_errors: 0,
    };

    let mut per_grab = Vec::with_capacity(count as usize);
    let mut hashes = Vec::with_capacity(count as usize);
    let mut sample = vec![0u8; SAMPLE_BYTES];
    let started = clock::now();

    for _ in 0..count {
        let mut info = FrameGrabInfo::default();
        let before = clock::now();
        let status = session.grab(&mut info, blocking);
        let after = clock::now();

        if status != 0 {
            result.failures += 1;
            continue;
        }
        result.grabs += 1;
        per_grab.push(clock::ticks_to_ns(after - before));

        result.width = info.width;
        result.height = info.height;
        result.is_hdr |= info.is_hdr();
        if info.wait_mode_used != 0 {
            result.blocking_grabs += 1;
        }
        if !info.driver_ok() {
            result.driver_errors += 1;
        }

        // Outside the timed region, and only a few kilobytes of it.
        if session.cuda.memcpy_dtoh(&mut sample, session.buffer) == CUDA_SUCCESS {
            hashes.push(sample_hash(sample.as_ptr(), sample.len()));
        }
    }

    result.elapsed_ns = clock::ticks_to_ns(clock::now() - started);
    result.overhead_ns = result.elapsed_ns.saturating_sub(per_grab.iter().sum::<u64>());

    per_grab.sort_unstable();
    if !per_grab.is_empty() {
        result.p50_ns = per_grab[per_grab.len() / 2];
        result.p99_ns = per_grab[per_grab.len() * 99 / 100];
    }
    hashes.sort_unstable();
    hashes.dedup();
    result.unique = hashes.len();

    result
}

// ---------------------------------------------------------------- latency watcher

/// A ToCuda session presented as a [`crate::watch::Watcher`].
///
/// Lives here rather than in `watch.rs` because reading the signal needs the
/// session's device pointer and the pitch from its own grab info, both of which
/// are private to this module.
pub struct Watch<'a> {
    session: ToCuda<'a>,
    x: u32,
    y: u32,
}

impl<'a> Watch<'a> {
    pub fn open(cuda: &'a Cuda, x: u32, y: u32, ten_bit: bool, hdr: bool) -> Option<Watch<'a>> {
        Some(Watch {
            session: open(cuda, ten_bit, hdr)?,
            x,
            y,
        })
    }
}

impl crate::watch::Watcher for Watch<'_> {
    fn name(&self) -> &'static str {
        "NvFBC "
    }

    fn poll(&mut self, out: &mut [u8; crate::readback::SAMPLE_BYTES]) -> Result<bool, String> {
        let mut info = FrameGrabInfo::default();
        let status = self.session.grab(&mut info, false);
        if status != 0 {
            return Err(format!("NvFBCCudaGrabFrame: {}", result_name(status)));
        }
        if info.buffer_width == 0 {
            return Ok(false);
        }

        // ARGB and ARGB10 are both 32bpp, so the pitch is the padded width in
        // pixels times four whichever format the session was set up with.
        let pitch = u64::from(info.buffer_width) * 4;
        let offset = u64::from(self.y) * pitch + u64::from(self.x) * 4;
        let status = self
            .session
            .cuda
            .memcpy_dtoh(out, self.session.buffer + offset);
        if status != CUDA_SUCCESS {
            return Err(format!("cuMemcpyDtoH at the read point: {status}"));
        }
        Ok(true)
    }
}

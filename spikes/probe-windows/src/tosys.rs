// SPDX-License-Identifier: GPL-2.0-or-later

//! Driving `INvFBCToSys` — the part that turns "a session was created" into a
//! number.
//!
//! # What this is for
//!
//! CLAUDE.md budgets ~16.7ms for DWM composition, the largest line in the
//! latency table, and says only NvFBC or swapchain hooking avoids it. That claim
//! has never been measured, and it is what justifies both this backend and the
//! Phase 7 hook. DDA and WGC are post-composition and therefore refresh-capped;
//! if NvFBC returns *unique* frames faster than the display refreshes, it is not
//! composition-bound and the claim is real. If it caps at refresh, the claim is
//! wrong and should come out of the budget.
//!
//! So the measurement that matters is not "does it work" but **achieved unique
//! frames per second**, which is why the loop below hashes what it captured.
//! Reporting 187 fps that turn out to be 187 copies of one frame would be worse
//! than reporting nothing.
//!
//! # Layout and vtable
//!
//! Structs and constants are transcribed from the NVIDIA Capture SDK's
//! `inc/NvFBC/nvFBCToSys.h` and `nvFBC.h` (`NVFBC_DLL_VERSION 0x70`, matching
//! what the driver reports). The headers are not vendored here: they carry
//! NVIDIA's "Licensed Deliverables" notice and this repo is GPL-2.0-or-later, so
//! the same rule CLAUDE.md applies to the Xbox dongle firmware applies here —
//! use them, keep them out of the tree.
//!
//! `NvFBC_CreateEx` returns a C++ object rather than a handle, so calls go
//! through its vtable. The slot order is **read, not guessed**:
//! `class INvFBCToSys_v4` declares five pure virtuals, single inheritance, no
//! declared virtual destructor, so under MSVC the slot index is the declaration
//! index. Getting this wrong calls the wrong function with the right arguments,
//! which is not a crash you can read, so the order is spelled out in
//! [`Slot`] rather than left as bare integers.

use std::ffi::c_void;

use sunburst_core::instr::clock;

use crate::nvfbc::{self, result_name};

/// Vtable slots of `INvFBCToSys_v4`, in declaration order.
#[repr(usize)]
enum Slot {
    SetUp = 0,
    GrabFrame = 1,
    #[expect(dead_code, reason = "documents the vtable; slot order is the point")]
    CursorCapture = 2,
    #[expect(dead_code, reason = "documents the vtable; slot order is the point")]
    GpuBasedCpuSleep = 3,
    Release = 4,
}

/// `NVFBCToSysBufferFormat`.
const NVFBC_TOSYS_ARGB: u32 = 0;
/// Added in Capture SDK 6.0. Documented as **A2B10G10R10, 32bpp** — an integer
/// format, not the scRGB FP16 CLAUDE.md's shader notes assume.
const NVFBC_TOSYS_ARGB10: u32 = 6;

/// `NVFBCToSysGrabMode::NVFBC_TOSYS_SOURCEMODE_FULL`.
const NVFBC_TOSYS_SOURCEMODE_FULL: u32 = 0;

/// `NVFBC_TOSYS_GRAB_FLAGS::NVFBC_TOSYS_NOWAIT`.
///
/// Without this the grab blocks until the desktop changes, which would measure
/// how often the desktop changes rather than what capture costs.
const NVFBC_TOSYS_NOWAIT: u32 = 0x1;

/// `bHDRRequest`, bit 3 of the setup params bitfield — after `bWithHWCursor`,
/// `bDiffMap` and `bEnableSeparateCursorCapture`.
const SETUP_FLAG_HDR_REQUEST: u32 = 1 << 3;

/// `NvFBCFrameGrabInfo`, 0x70 layout.
///
/// Differs from the 0x50 one: `bIsHDR`, `bReservedBit1`, `bReservedBits:30` and
/// `dwWaitModeUsed` were added, and `dwReserved2` shrank to 11.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FrameGrabInfo {
    width: u32,
    height: u32,
    buffer_width: u32,
    reserved: u32,
    overlay_active: i32,
    must_recreate: i32,
    first_buffer: i32,
    hw_mouse_visible: i32,
    protected_content: i32,
    driver_internal_error: u32,
    stereo_on: i32,
    igpu_capture: i32,
    source_pid: u32,
    reserved3: u32,
    /// `bIsHDR:1`, `bReservedBit1:1`, `bReservedBits:30`.
    flags: u32,
    wait_mode_used: u32,
    reserved2: [u32; 11],
}

const GRAB_INFO_SIZE: usize = size_of::<FrameGrabInfo>();
const _: () = assert!(GRAB_INFO_SIZE == 108);
const _: () = assert!(std::mem::offset_of!(FrameGrabInfo, flags) == 56);
const _: () = assert!(std::mem::offset_of!(FrameGrabInfo, wait_mode_used) == 60);

impl FrameGrabInfo {
    fn is_hdr(&self) -> bool {
        self.flags & 1 != 0
    }
    /// `dwDriverInternalError`: 0 or the sentinel both mean "nothing to report".
    fn driver_ok(&self) -> bool {
        self.driver_internal_error == 0 || self.driver_internal_error == 0xFBCA_11F9
    }
}

/// `NVFBC_TOSYS_SETUP_PARAMS_V3`.
#[repr(C)]
struct SetupParams {
    version: u32,
    /// `bWithHWCursor:1`, `bDiffMap:1`, `bEnableSeparateCursorCapture:1`,
    /// `bHDRRequest:1`, `bClassificationMap:1`, then 27 reserved.
    flags: u32,
    mode: u32,
    diffmap_block_size: u32,
    classification_stamp_width: u32,
    classification_stamp_height: u32,
    pp_buffer: *mut *mut c_void,
    pp_diffmap: *mut *mut c_void,
    cursor_capture_event: *mut c_void,
    pp_classification_map: *mut *mut c_void,
    reserved: [u32; 56],
    reserved_ptrs: [*const c_void; 28],
}

const SETUP_SIZE: usize = size_of::<SetupParams>();
const _: () = assert!(SETUP_SIZE == 504);
const _: () = assert!(std::mem::offset_of!(SetupParams, mode) == 8);
const _: () = assert!(std::mem::offset_of!(SetupParams, pp_buffer) == 24);

impl Default for SetupParams {
    fn default() -> Self {
        SetupParams {
            version: 0,
            flags: 0,
            mode: 0,
            diffmap_block_size: 0,
            classification_stamp_width: 0,
            classification_stamp_height: 0,
            pp_buffer: std::ptr::null_mut(),
            pp_diffmap: std::ptr::null_mut(),
            cursor_capture_event: std::ptr::null_mut(),
            pp_classification_map: std::ptr::null_mut(),
            reserved: [0; 56],
            reserved_ptrs: [std::ptr::null(); 28],
        }
    }
}

/// `NVFBC_TOSYS_GRAB_FRAME_PARAMS_V1`.
#[repr(C)]
struct GrabParams {
    version: u32,
    flags: u32,
    target_width: u32,
    target_height: u32,
    start_x: u32,
    start_y: u32,
    grab_mode: u32,
    wait_time: u32,
    grab_info: *mut FrameGrabInfo,
    reserved: [u32; 56],
    reserved_ptrs: [*const c_void; 31],
}

const GRAB_SIZE: usize = size_of::<GrabParams>();
const _: () = assert!(GRAB_SIZE == 512);
const _: () = assert!(std::mem::offset_of!(GrabParams, grab_info) == 32);

impl Default for GrabParams {
    fn default() -> Self {
        GrabParams {
            version: 0,
            flags: 0,
            target_width: 0,
            target_height: 0,
            start_x: 0,
            start_y: 0,
            grab_mode: 0,
            wait_time: 0,
            grab_info: std::ptr::null_mut(),
            reserved: [0; 56],
            reserved_ptrs: [std::ptr::null(); 31],
        }
    }
}

type PfnSetUp = unsafe extern "system" fn(*mut c_void, *mut SetupParams) -> i32;
type PfnGrabFrame = unsafe extern "system" fn(*mut c_void, *mut GrabParams) -> i32;
type PfnRelease = unsafe extern "system" fn(*mut c_void) -> i32;

/// An `INvFBCToSys_v4` instance, released on drop.
pub struct ToSys {
    object: *mut c_void,
    /// NvFBC allocates the frame buffer and writes its address here during
    /// setup; it stays valid for the life of the session.
    buffer: *mut c_void,
}

impl ToSys {
    /// # Safety
    ///
    /// `object` must be a live `INvFBCToSys` from `NvFBC_CreateEx`, not already
    /// owned by another `ToSys`.
    pub unsafe fn new(object: *mut c_void) -> ToSys {
        ToSys {
            object,
            buffer: std::ptr::null_mut(),
        }
    }

    /// Read a vtable slot.
    ///
    /// # Safety
    ///
    /// `T` must match the declared signature of that slot in `INvFBCToSys_v4`.
    unsafe fn slot<T: Copy>(&self, slot: Slot) -> T {
        // SAFETY: the object's first word is its vtable pointer, and the caller
        // guarantees the signature for this slot.
        unsafe {
            let vtable = *(self.object as *const *const *const c_void);
            nvfbc::cast_fn(*vtable.add(slot as usize))
        }
    }

    fn setup(&mut self, ten_bit: bool, hdr: bool) -> i32 {
        let mut params = SetupParams {
            version: nvfbc::struct_version(SETUP_SIZE, 3),
            mode: if ten_bit {
                NVFBC_TOSYS_ARGB10
            } else {
                NVFBC_TOSYS_ARGB
            },
            pp_buffer: &raw mut self.buffer,
            ..Default::default()
        };
        if hdr {
            params.flags |= SETUP_FLAG_HDR_REQUEST;
        }

        // SAFETY: slot 0 is NvFBCToSysSetUp(NVFBC_TOSYS_SETUP_PARAMS_V3*), and
        // `params` is correctly laid out with its own size in `version`.
        let f: PfnSetUp = unsafe { self.slot(Slot::SetUp) };
        // SAFETY: `params` outlives the call and `self.buffer` outlives the
        // session, which is what `ppBuffer` requires.
        unsafe { f(self.object, &mut params) }
    }

    fn grab(&self, info: &mut FrameGrabInfo) -> i32 {
        let mut params = GrabParams {
            version: nvfbc::struct_version(GRAB_SIZE, 1),
            flags: NVFBC_TOSYS_NOWAIT,
            grab_mode: NVFBC_TOSYS_SOURCEMODE_FULL,
            grab_info: info,
            ..Default::default()
        };
        // SAFETY: slot 1 is NvFBCToSysGrabFrame(NVFBC_TOSYS_GRAB_FRAME_PARAMS*).
        let f: PfnGrabFrame = unsafe { self.slot(Slot::GrabFrame) };
        // SAFETY: both structs are correctly laid out and outlive the call.
        unsafe { f(self.object, &mut params) }
    }
}

impl Drop for ToSys {
    fn drop(&mut self) {
        if self.object.is_null() {
            return;
        }
        // SAFETY: slot 4 is NvFBCToSysRelease(), taking only the object.
        let f: PfnRelease = unsafe { self.slot(Slot::Release) };
        // SAFETY: the object is live and released exactly once, here.
        unsafe { f(self.object) };
        self.object = std::ptr::null_mut();
    }
}

/// One capture run's results.
pub struct Capture {
    pub setup_result: i32,
    pub grabs: u32,
    pub failures: u32,
    pub unique: usize,
    pub elapsed_ns: u64,
    pub p50_ns: u64,
    pub p99_ns: u64,
    pub width: u32,
    pub height: u32,
    pub is_hdr: bool,
    pub blocking_grabs: u32,
    pub driver_errors: u32,
}

impl Capture {
    pub fn fps(&self) -> f64 {
        if self.elapsed_ns == 0 {
            return 0.0;
        }
        f64::from(self.grabs) * 1e9 / self.elapsed_ns as f64
    }
}

/// Hash a sample of the frame, to tell a new frame from a repeat.
///
/// Deliberately a subsample: a 4K ARGB buffer is ~33 MB, and hashing all of it
/// per grab would cost more than the grab and swamp the number being measured.
/// ~4096 spread bytes is plenty to distinguish frames without becoming the
/// measurement itself. Runs outside the timed region regardless.
fn sample_hash(buffer: *const u8, len: usize) -> u64 {
    if buffer.is_null() || len == 0 {
        return 0;
    }
    let stride = (len / 4096).max(1);
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut offset = 0;
    while offset < len {
        // SAFETY: `offset < len` and the buffer is at least `len` bytes, owned
        // by NvFBC for the life of the session.
        let byte = unsafe { *buffer.add(offset) };
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        offset += stride;
    }
    hash
}

/// Set up and grab `count` frames, timing each grab.
pub fn run(session: &mut ToSys, ten_bit: bool, hdr: bool, count: u32) -> Capture {
    let mut result = Capture {
        setup_result: session.setup(ten_bit, hdr),
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
    if result.setup_result != 0 {
        return result;
    }

    let mut per_grab = Vec::with_capacity(count as usize);
    let mut hashes = Vec::with_capacity(count as usize);
    let started = clock::now();

    for _ in 0..count {
        let mut info = FrameGrabInfo::default();
        let before = clock::now();
        let status = session.grab(&mut info);
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

        // Outside the timed region on purpose.
        let bytes = info.buffer_width as usize * info.height as usize * 4;
        hashes.push(sample_hash(session.buffer.cast(), bytes));
    }

    result.elapsed_ns = clock::ticks_to_ns(clock::now() - started);

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

/// Print one capture run.
pub fn report(label: &str, capture: &Capture) {
    if capture.setup_result != 0 {
        println!(
            "  {label}: SetUp failed -- {}",
            result_name(capture.setup_result)
        );
        return;
    }
    println!(
        "  {label}: {}x{}, {} grabs ({} failed)",
        capture.width, capture.height, capture.grabs, capture.failures
    );
    println!(
        "      {:.1} fps, {} unique frame(s), per-grab p50 {:.2}ms p99 {:.2}ms",
        capture.fps(),
        capture.unique,
        capture.p50_ns as f64 / 1e6,
        capture.p99_ns as f64 / 1e6,
    );
    if capture.blocking_grabs > 0 {
        println!(
            "      {} of {} grabs actually blocked despite NOWAIT (dwWaitModeUsed)",
            capture.blocking_grabs, capture.grabs
        );
    }
    if capture.driver_errors > 0 {
        println!(
            "      {} grab(s) reported dwDriverInternalError",
            capture.driver_errors
        );
    }
}

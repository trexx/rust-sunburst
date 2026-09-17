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

use crate::capture::{Capture, FrameGrabInfo, sample_hash};
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

/// `NVFBC_TOSYS_GRAB_FLAGS::NVFBC_TOSYS_NOWAIT`. Returns whatever is there,
/// new or not, which is what exposes the unconditional copy cost.
const NVFBC_TOSYS_NOWAIT: u32 = 0x1;

/// `NVFBC_TOSYS_NOFLAGS`: wait for a genuinely new frame.
///
/// Needed for a fair rate comparison. Polling with NOWAIT re-copies stale frames
/// at full cost, so it measures how often we asked, not how fast NvFBC can
/// deliver — the first comparison against DDA was depressed by exactly that.
const NVFBC_TOSYS_NOFLAGS: u32 = 0x0;

/// `bHDRRequest`, bit 3 of the setup params bitfield — after `bWithHWCursor`,
/// `bDiffMap` and `bEnableSeparateCursorCapture`.
const SETUP_FLAG_HDR_REQUEST: u32 = 1 << 3;

/// Which shape of setup call to try.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Baseline,
    NonNullOutPtrs,
    WithHwCursor,
    LegacyV2,
}

impl Variant {
    pub const ALL: [Variant; 4] = [
        Variant::Baseline,
        Variant::NonNullOutPtrs,
        Variant::WithHwCursor,
        Variant::LegacyV2,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Variant::Baseline => "baseline      ",
            Variant::NonNullOutPtrs => "non-null outs ",
            Variant::WithHwCursor => "with HW cursor",
            Variant::LegacyV2 => "legacy V2     ",
        }
    }
}

/// `NVFBC_TOSYS_SETUP_PARAMS_V2`, the 0x50-era shape.
///
/// Same 504 bytes as V3 but a different field order, so a version mismatch here
/// reads pointers from the wrong offsets rather than failing a size check.
#[repr(C)]
struct LegacyV2Params {
    version: u32,
    flags: u32,
    mode: u32,
    reserved1: u32,
    pp_buffer: *mut *mut c_void,
    pp_diffmap: *mut *mut c_void,
    cursor_capture_event: *mut c_void,
    reserved: [u32; 58],
    reserved_ptrs: [*const c_void; 29],
}
const _: () = assert!(size_of::<LegacyV2Params>() == 504);

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
    /// Landing slots for [`Variant::NonNullOutPtrs`], so the driver has
    /// somewhere real to write even though the matching features are off.
    spare_diffmap: *mut c_void,
    spare_classification: *mut c_void,
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
            spare_diffmap: std::ptr::null_mut(),
            spare_classification: std::ptr::null_mut(),
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

    /// One setup attempt, shaped by `variant`.
    ///
    /// The variants exist because the first run came back `ERROR_INVALID_PTR`
    /// from a call whose parameters match NVIDIA's own ToSys sample field for
    /// field, with struct sizes and offsets independently confirmed against the
    /// MSVC ABI. When the obvious reading is exhausted, asking the driver which
    /// shape it accepts beats guessing again.
    fn setup_with(&mut self, variant: Variant, ten_bit: bool, hdr: bool) -> i32 {
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
        match variant {
            Variant::Baseline => {}
            // In case the driver dereferences these regardless of the flags
            // that are supposed to gate them.
            Variant::NonNullOutPtrs => {
                params.pp_diffmap = &raw mut self.spare_diffmap;
                params.pp_classification_map = &raw mut self.spare_classification;
            }
            Variant::WithHwCursor => params.flags |= 1,
            // The 0x50-era struct is a different shape at the same 504 bytes, so
            // a driver expecting it would read our pointers from the wrong
            // offsets -- which would present as exactly this error.
            Variant::LegacyV2 => {
                params = SetupParams {
                    version: nvfbc::struct_version(SETUP_SIZE, 2),
                    ..Default::default()
                };
                let v2 = (&raw mut params).cast::<LegacyV2Params>();
                // SAFETY: `params` is 504 bytes of zeroed, writable storage, and
                // LegacyV2Params is the same size -- asserted below.
                unsafe {
                    (*v2).version = nvfbc::struct_version(SETUP_SIZE, 2);
                    (*v2).mode = if ten_bit {
                        NVFBC_TOSYS_ARGB10
                    } else {
                        NVFBC_TOSYS_ARGB
                    };
                    (*v2).pp_buffer = &raw mut self.buffer;
                    if hdr {
                        // Rebuilding `params` above dropped the flag set before
                        // the match, which is why the first HDR run reported
                        // "bIsHDR clear" -- it had never asked.
                        //
                        // Bit 3 is INFERRED, not read. It is bHDRRequest in the
                        // V3 struct this driver rejects; in the 5.0-era V2 it is
                        // still inside bReservedBits. SDK 6.0 added HDR while V2
                        // was current, so it most likely took this bit, but that
                        // header is not on hand. A wrong guess sets a reserved
                        // bit, which is why the result is reported rather than
                        // trusted.
                        (*v2).flags |= SETUP_FLAG_HDR_REQUEST;
                    }
                }
            }
        }

        // SAFETY: slot 0 is NvFBCToSysSetUp(NVFBC_TOSYS_SETUP_PARAMS_V3*), and
        // `params` is correctly laid out with its own size in `version`.
        let f: PfnSetUp = unsafe { self.slot(Slot::SetUp) };
        // SAFETY: `params` outlives the call and `self.buffer` outlives the
        // session, which is what `ppBuffer` requires.
        unsafe { f(self.object, &mut params) }
    }

    fn grab(&self, info: &mut FrameGrabInfo, blocking: bool) -> i32 {
        let mut params = GrabParams {
            version: nvfbc::struct_version(GRAB_SIZE, 1),
            flags: if blocking {
                NVFBC_TOSYS_NOFLAGS
            } else {
                NVFBC_TOSYS_NOWAIT
            },
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

/// Release an `INvFBCToSys` without wrapping it.
///
/// # Safety
///
/// `object` must be a live instance from `NvFBC_CreateEx`, not owned by a
/// [`ToSys`], and must not be used afterwards.
pub(crate) unsafe fn release(object: *mut c_void) {
    // SAFETY: slot 4 is NvFBCToSysRelease(), taking only the object.
    unsafe {
        let vtable = *(object as *const *const *const c_void);
        let f: PfnRelease = nvfbc::cast_fn(*vtable.add(Slot::Release as usize));
        f(object);
    }
}

/// Report where each vtable slot actually points.
///
/// If slot 0 is not inside `NvFBC64.dll`, the vtable read is wrong and every
/// conclusion drawn from a call through it is worthless. Cheap to check, and it
/// separates "our pointer arithmetic is wrong" from "the driver refused".
pub fn report_vtable(session: &ToSys) {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        GetModuleFileNameA, GetModuleHandleExA,
    };

    println!("  vtable at {:p}:", session.object);
    for index in 0..5usize {
        // SAFETY: the object's first word is its vtable pointer; slots 0..5 are
        // the five virtuals INvFBCToSys_v4 declares.
        let entry = unsafe {
            let vtable = *(session.object as *const *const *const c_void);
            *vtable.add(index)
        };

        let mut module = HMODULE::default();
        // SAFETY: `entry` is only used as an address to attribute, never called.
        let owner = unsafe {
            GetModuleHandleExA(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                    | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                windows::core::PCSTR(entry.cast()),
                &mut module,
            )
        };

        let mut name = [0u8; 260];
        let owner = if owner.is_ok() {
            // SAFETY: `module` is a live handle and `name` is a valid buffer.
            let len = unsafe { GetModuleFileNameA(Some(module), &mut name) } as usize;
            String::from_utf8_lossy(&name[..len.min(name.len())])
                .rsplit('\\')
                .next()
                .unwrap_or("?")
                .to_string()
        } else {
            "NOT IN ANY LOADED MODULE".to_string()
        };
        println!("    slot {index}: {entry:p}  {owner}");
    }
}

/// Try each setup shape, reporting every result, then hand back a fresh session
/// configured with whichever was accepted.
///
/// Each attempt is created and released before the next: **NvFBC allows one
/// session at a time**, so holding a winner open while testing the rest would
/// make every later create fail — which is exactly what it did the first time
/// this ran.
pub fn probe_variants(ten_bit: bool, hdr: bool) -> Option<(ToSys, Variant)> {
    let mut winner = None;
    let mut dumped = false;

    for variant in Variant::ALL {
        let created = nvfbc::create_session();
        if !created.succeeded {
            println!(
                "    {}: no session -- CreateEx said {}",
                variant.name(),
                result_name(created.result)
            );
            continue;
        }
        // SAFETY: a fresh live object from NvFBC_CreateEx, wrapped once, and
        // released when this binding drops at the end of the iteration.
        let mut session = unsafe { ToSys::new(created.object) };
        if !dumped {
            report_vtable(&session);
            dumped = true;
        }
        let status = session.setup_with(variant, ten_bit, hdr);
        println!("    {}: {}", variant.name(), result_name(status));
        if status == 0 && winner.is_none() {
            winner = Some(variant);
        }
    }

    // Only now, with every trial session released, take one for real.
    let variant = winner?;
    open(variant, ten_bit, hdr).map(|session| (session, variant))
}

/// Open one session in a known-good shape.
///
/// Any previously opened session must already be dropped — one at a time.
pub fn open(variant: Variant, ten_bit: bool, hdr: bool) -> Option<ToSys> {
    let created = nvfbc::create_session();
    if !created.succeeded {
        println!(
            "    no session for {}: {}",
            variant.name().trim(),
            result_name(created.result)
        );
        return None;
    }
    // SAFETY: a fresh live object, wrapped once; the caller owns it.
    let mut session = unsafe { ToSys::new(created.object) };
    let status = session.setup_with(variant, ten_bit, hdr);
    if status != 0 {
        println!("    SetUp failed for that shape: {}", result_name(status));
        return None;
    }
    Some(session)
}

/// Grab `count` frames from an already-set-up session, timing each grab.
///
/// `blocking` waits for a new frame each time, which measures the delivery rate.
/// Polling instead measures the cost of asking.
pub fn run(session: &mut ToSys, count: u32, blocking: bool) -> Capture {
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
    if result.setup_result != 0 {
        return result;
    }

    let mut per_grab = Vec::with_capacity(count as usize);
    let mut hashes = Vec::with_capacity(count as usize);
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

        // Outside the timed region on purpose.
        let bytes = info.buffer_width as usize * info.height as usize * 4;
        hashes.push(sample_hash(session.buffer.cast(), bytes));
    }

    result.elapsed_ns = clock::ticks_to_ns(clock::now() - started);
    result.overhead_ns = result
        .elapsed_ns
        .saturating_sub(per_grab.iter().sum::<u64>());

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

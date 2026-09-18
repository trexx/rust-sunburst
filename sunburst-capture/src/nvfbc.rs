// SPDX-License-Identifier: GPL-2.0-or-later

//! Legacy NvFBC (`NvFBC_CreateEx`) — the keyed session create, promoted from the
//! Phase-0 probe.
//!
//! Only the create path the CUDA backend needs is kept here; the probe's
//! detection/measurement half (status, SDK version, both-ways reporting) lived in
//! the spike and does not belong in a backend. NvFBC is **opt-in resilience
//! only** — a failed `create_interface` is the driver's answer, surfaced as a
//! backend error, and the owner falls back to DDA/WGC.
//!
//! # The private-data key
//!
//! Legacy NvFBC is gated to professional cards; `NvFBC_CreateEx` refuses on a
//! GeForce unless the caller passes [`MAGIC`], the well-known `nvidia-patch` key.
//! It is a key, not a computation — nothing to derive.
//!
//! # Layout is ABI
//!
//! `NVFBC_STRUCT_VERSION` ors `sizeof` into the version word, so a transcription
//! slip is an `INCOMPATIBLE_VERSION` from the driver, not a silent bug. The
//! `const` asserts pin the structs to the 512 / 108 bytes the headers produce on
//! 64-bit Windows, catching a slip at compile time.

use std::ffi::{CString, c_void};

use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
use windows::core::PCSTR;

/// `NVFBC_DLL_VERSION` from `nvFBC.h` (Capture SDK 7.x — the box reports `0x70`).
const NVFBC_DLL_VERSION: u32 = 0x70;

/// `NVFBC_STRUCT_VERSION(typeName, ver)`: `sizeof | ver<<16 | DLL_VERSION<<24`.
pub(crate) const fn struct_version(size: usize, ver: u32) -> u32 {
    (size as u32) | (ver << 16) | (NVFBC_DLL_VERSION << 24)
}

/// The private-data key legacy NvFBC accepts on a GeForce (the `nvidia-patch`
/// value). A key, not a computation.
const MAGIC: [u32; 4] = [0xAEF5_7AC5, 0x401D_1A39, 0x1B85_6BBE, 0x9ED0_CEBA];

/// `NvFBCCreateParams` from `nvFBC.h`.
#[repr(C)]
struct NvFbcCreateParams {
    version: u32,
    interface_type: u32,
    max_display_width: u32,
    max_display_height: u32,
    device: *const c_void,
    private_data: *const c_void,
    private_data_size: u32,
    interface_version: u32,
    nvfbc: *mut c_void,
    adapter_idx: u32,
    nvfbc_version: u32,
    cuda_ctx: *const c_void,
    private_data2: *const c_void,
    private_data2_size: u32,
    reserved: [u32; 55],
    reserved_ptrs: [*const c_void; 27],
}

impl Default for NvFbcCreateParams {
    fn default() -> Self {
        NvFbcCreateParams {
            version: 0,
            interface_type: 0,
            max_display_width: 0,
            max_display_height: 0,
            device: std::ptr::null(),
            private_data: std::ptr::null(),
            private_data_size: 0,
            interface_version: 0,
            nvfbc: std::ptr::null_mut(),
            adapter_idx: 0,
            nvfbc_version: 0,
            cuda_ctx: std::ptr::null(),
            private_data2: std::ptr::null(),
            private_data2_size: 0,
            reserved: [0; 55],
            reserved_ptrs: [std::ptr::null(); 27],
        }
    }
}

const CREATE_SIZE: usize = size_of::<NvFbcCreateParams>();
const _: () = assert!(CREATE_SIZE == 512);
const _: () = assert!(std::mem::offset_of!(NvFbcCreateParams, interface_type) == 4);
const _: () = assert!(std::mem::offset_of!(NvFbcCreateParams, private_data) == 24);
const _: () = assert!(std::mem::offset_of!(NvFbcCreateParams, private_data_size) == 32);
const _: () = assert!(std::mem::offset_of!(NvFbcCreateParams, nvfbc) == 40);

/// `NvFBCFrameGrabInfo`, 0x70 layout (108 bytes). Returned by every grab; the
/// fields the CUDA backend reads are `pub(crate)`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct FrameGrabInfo {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) buffer_width: u32,
    reserved: u32,
    overlay_active: i32,
    pub(crate) must_recreate: i32,
    first_buffer: i32,
    hw_mouse_visible: i32,
    protected_content: i32,
    pub(crate) driver_internal_error: u32,
    stereo_on: i32,
    igpu_capture: i32,
    source_pid: u32,
    reserved3: u32,
    /// `bIsHDR:1`, then reserved.
    pub(crate) flags: u32,
    pub(crate) wait_mode_used: u32,
    reserved2: [u32; 11],
}

const GRAB_INFO_SIZE: usize = size_of::<FrameGrabInfo>();
const _: () = assert!(GRAB_INFO_SIZE == 108);
const _: () = assert!(std::mem::offset_of!(FrameGrabInfo, flags) == 56);
const _: () = assert!(std::mem::offset_of!(FrameGrabInfo, wait_mode_used) == 60);

impl FrameGrabInfo {
    pub(crate) fn is_hdr(&self) -> bool {
        self.flags & 1 != 0
    }
    /// `dwDriverInternalError`: 0 or the sentinel both mean "nothing to report".
    pub(crate) fn driver_ok(&self) -> bool {
        self.driver_internal_error == 0 || self.driver_internal_error == 0xFBCA_11F9
    }
}

type PfnCreateEx = unsafe extern "system" fn(*mut c_void) -> i32;

/// One `NvFBC_CreateEx` attempt.
pub(crate) struct Create {
    pub(crate) result: i32,
    /// The interface instance (e.g. `INvFBCCuda`). Null on failure.
    pub(crate) object: *mut c_void,
    pub(crate) succeeded: bool,
}

/// Names the Capture SDK has shipped the 64-bit runtime under.
const CANDIDATES: [&str; 2] = ["NvFBC64.dll", "nvfbc64.dll"];

fn load(name: &str) -> Option<HMODULE> {
    let cname = CString::new(name).ok()?;
    // SAFETY: `cname` is NUL-terminated and outlives the call.
    unsafe { LoadLibraryA(PCSTR(cname.as_ptr().cast())) }.ok()
}

fn symbol(module: HMODULE, name: &str) -> Option<*const c_void> {
    let cname = CString::new(name).ok()?;
    // SAFETY: `module` is a live handle and `cname` is NUL-terminated.
    unsafe { GetProcAddress(module, PCSTR(cname.as_ptr().cast())) }.map(|p| p as *const c_void)
}

/// Reinterpret an export or vtable slot as a function pointer.
///
/// # Safety
///
/// `T` must be the exact signature the DLL exports / the vtable declares. Every
/// call site is transcribed from `nvFBC.h`.
pub(crate) unsafe fn cast_fn<T: Copy>(ptr: *const c_void) -> T {
    debug_assert_eq!(size_of::<T>(), size_of::<*const c_void>());
    // SAFETY: the caller guarantees the signature; a code and a data pointer are
    // the same width on every target this runs on.
    unsafe { *(&ptr as *const *const c_void).cast::<T>() }
}

/// Create one **keyed** session of the given interface type, for the caller to
/// own and release. Returns the attempt either way, so a failure carries its
/// actual result code.
pub(crate) fn create_interface(interface_type: u32) -> Create {
    let failed = || Create {
        result: -1,
        object: std::ptr::null_mut(),
        succeeded: false,
    };
    let Some(module) = CANDIDATES.into_iter().find_map(load) else {
        return failed();
    };
    let Some(p) = symbol(module, "NvFBC_CreateEx") else {
        return failed();
    };
    // SAFETY: signature transcribed from nvFBC.h.
    let f: PfnCreateEx = unsafe { cast_fn(p) };

    let mut params = NvFbcCreateParams {
        version: struct_version(CREATE_SIZE, 2),
        interface_type,
        private_data: MAGIC.as_ptr().cast(),
        private_data_size: size_of_val(&MAGIC) as u32,
        ..Default::default()
    };
    // SAFETY: `params` is fully initialised, its `version` encodes its own size,
    // `pDevice`/`cudaCtx` are null (which the CUDA interface permits — it makes
    // its own context), and `MAGIC` outlives the call.
    let result = unsafe { f((&raw mut params).cast()) };
    Create {
        result,
        object: params.nvfbc,
        succeeded: result == 0 && !params.nvfbc.is_null(),
    }
}

/// `NVFBCRESULT` names, from `nvFBC.h`.
pub(crate) fn result_name(code: i32) -> &'static str {
    match code {
        0 => "NVFBC_SUCCESS",
        -1 => "ERROR_GENERIC",
        -2 => "ERROR_INVALID_PARAM",
        -3 => "ERROR_INVALIDATED_SESSION",
        -4 => "ERROR_PROTECTED_CONTENT",
        -5 => "ERROR_DRIVER_FAILURE",
        -6 => "ERROR_CUDA_FAILURE",
        -7 => "ERROR_UNSUPPORTED",
        -8 => "ERROR_HW_ENC_FAILURE",
        -9 => "ERROR_INCOMPATIBLE_DRIVER",
        -10 => "ERROR_UNSUPPORTED_PLATFORM",
        -11 => "ERROR_OUT_OF_MEMORY",
        -12 => "ERROR_INVALID_PTR",
        -13 => "ERROR_INCOMPATIBLE_VERSION",
        -14 => "ERROR_OPT_CAPTURE_FAILURE",
        -15 => "ERROR_INSUFFICIENT_PRIVILEGES",
        _ => "ERROR_UNKNOWN",
    }
}

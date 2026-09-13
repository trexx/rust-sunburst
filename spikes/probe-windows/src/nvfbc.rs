// SPDX-License-Identifier: GPL-2.0-or-later

//! Phase 0.1's actual question: is NvFBC *usable* on this driver?
//!
//! # Why this file has two halves
//!
//! The first version of this probe asked for `NvFBCCreateInstance` and, not
//! finding it, called NvFBC unavailable. That was the wrong question. There are
//! two generations of NvFBC and only one of them ever shipped on Windows:
//!
//! - **NvFBC 7.x** (`NvFBCCreateInstance`, a function table) — the Linux one.
//!   Its absence from `NvFBC64.dll` says nothing about whether capture works.
//! - **Legacy NvFBC** (`NvFBC_CreateEx`, `NvFBC_GetStatusEx`, `NvFBC_Enable`) —
//!   the Windows API, and the one the driver actually still exports.
//!
//! So a missing modern entry point is not a negative result, it is a *version
//! detection*. Both are now reported, because "which generation is present"
//! changes which of the answers below mean anything.
//!
//! # The private-data key
//!
//! Legacy NvFBC is gated to professional cards. `NvFBCStatusEx::bIsCapturePossible`
//! comes back 0 on a GeForce, and `NvFBC_CreateEx` refuses, unless the caller
//! passes a private-data blob the driver accepts. That key is well known and is
//! what `nvidia-patch`-style shims inject; passing it is the difference between
//! "NvFBC does not work on this card" and "NvFBC is switched off on this card".
//!
//! Phase 0.1 needs to know which of those two it is, so the probe asks **both
//! ways and reports the pair**. A keyed success next to an unkeyed failure is
//! the only evidence that says the key is what mattered — a single keyed call
//! proves nothing, because a Quadro would pass either way.
//!
//! This does not make NvFBC a supportable backend on its own; see
//! `HARDWARE_TESTING.md` §1 for what a positive result here does and does not
//! license. The probe's job is to produce the fact, not to decide with it.
//!
//! # Layout
//!
//! The two parameter structs are transcribed field-for-field from NVIDIA's
//! `nvFBC.h` (Capture SDK, `NVFBC_DLL_VERSION 0x50`). Their size is *part of the
//! ABI* — `NVFBC_STRUCT_VERSION` ors `sizeof` into the version word — so a
//! transcription slip is not a silent memory bug, it is an
//! `NVFBC_ERROR_INCOMPATIBLE_VERSION` from the driver. The `const` asserts below
//! pin both to the 512 bytes the header produces on 64-bit Windows, so a slip is
//! caught at compile time instead.

use std::ffi::{CString, c_void};

use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
use windows::core::PCSTR;

/// `NVFBC_DLL_VERSION` from `nvFBC.h`.
///
/// Capture SDK 7.x, which is the generation the box reports: `GetSDKVersion`
/// comes back `0x70`. An earlier transcription used `0x50` and worked, because
/// the two structs it declared happen to be byte-identical across the two SDKs
/// and the driver accepts the older word. The ToSys structs below are *not*
/// identical across them, so matching the runtime is no longer optional.
const NVFBC_DLL_VERSION: u32 = 0x70;

/// `NVFBC_STRUCT_VERSION(typeName, ver)`: `sizeof | ver<<16 | DLL_VERSION<<24`.
pub(crate) const fn struct_version(size: usize, ver: u32) -> u32 {
    (size as u32) | (ver << 16) | (NVFBC_DLL_VERSION << 24)
}

/// `NVFBC_TO_SYS`, from `nvFBCToSys.h`. The cheapest interface to ask for: it
/// needs no D3D device, so a failure is the driver's answer and not ours.
const NVFBC_TO_SYS: u32 = 0x1204;

/// `NVFBC_STATE_ENABLE`.
const NVFBC_STATE_ENABLE: i32 = 1;

/// `NVFBC_GLOBAL_FLAGS_NO_INITIAL_REFRESH`. Set before creating so the probe
/// does not make the desktop repaint on its way past.
const NVFBC_GLOBAL_FLAGS_NO_INITIAL_REFRESH: u32 = 0x0000_0002;

/// The private-data key legacy NvFBC accepts on a GeForce.
///
/// Same value the `nvidia-patch`-derived Windows shims inject. It is a key, not
/// a computation — there is nothing to derive and nothing to keep in sync.
const MAGIC: [u32; 4] = [0xAEF5_7AC5, 0x401D_1A39, 0x1B85_6BBE, 0x9ED0_CEBA];

/// `NvFBCStatusEx` from `nvFBC.h`.
///
/// The five `[out]` booleans are a bitfield in the header, so they arrive packed
/// into one word here rather than as separate fields.
#[repr(C)]
struct NvFbcStatusEx {
    version: u32,
    /// `bIsCapturePossible:1`, `bCurrentlyCapturing:1`, `bCanCreateNow:1`,
    /// `bSupportMultiHead:1`, `bSupportConfigurableDiffMap:1`,
    /// `bSupportImageClassification:1`, then 26 reserved.
    ///
    /// Bits 4 and 5 differ from the 0x50 header: what was `bSupportMultiClient`
    /// is now `bSupportConfigurableDiffMap`, and bit 5 is new. The struct is the
    /// same size either way, so nothing errored — the earlier run simply printed
    /// bit 4 under the wrong name.
    flags: u32,
    nvfbc_version: u32,
    adapter_idx: u32,
    private_data: *const c_void,
    private_data_size: u32,
    reserved: [u32; 59],
    reserved_ptrs: [*const c_void; 31],
}

// `nvFBC.h`'s own callers memset these to zero and set only the fields they
// mean; arrays this long do not derive `Default`, so that idiom is written out.
impl Default for NvFbcStatusEx {
    fn default() -> Self {
        NvFbcStatusEx {
            version: 0,
            flags: 0,
            nvfbc_version: 0,
            adapter_idx: 0,
            private_data: std::ptr::null(),
            private_data_size: 0,
            reserved: [0; 59],
            reserved_ptrs: [std::ptr::null(); 31],
        }
    }
}

const STATUS_SIZE: usize = size_of::<NvFbcStatusEx>();
// `sizeof` is part of the version word, so the layout is checked here rather
// than discovered as a driver error on the box.
const _: () = assert!(STATUS_SIZE == 512);
// Size alone would still pass with two fields transposed, and the ones that
// matter are the pair carrying the key.
const _: () = assert!(std::mem::offset_of!(NvFbcStatusEx, private_data) == 16);
const _: () = assert!(std::mem::offset_of!(NvFbcStatusEx, private_data_size) == 24);

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

// `NVFBCAPI` is `__stdcall`, which on x86_64 Windows *is* the one calling
// convention; `extern "system"` names it correctly on both widths.
type PfnGetSdkVersion = unsafe extern "system" fn(*mut u32) -> i32;
type PfnGetStatusEx = unsafe extern "system" fn(*mut NvFbcStatusEx) -> i32;
type PfnEnable = unsafe extern "system" fn(i32) -> i32;
type PfnCreateEx = unsafe extern "system" fn(*mut c_void) -> i32;
type PfnSetGlobalFlags = unsafe extern "system" fn(u32);

/// Which generation of NvFBC the loaded runtime is.
#[derive(Debug, Default, PartialEq, Eq)]
pub enum Generation {
    /// `NvFBCCreateInstance` — the 7.x/Linux-shaped API.
    Modern,
    /// `NvFBC_CreateEx` and friends — the Windows API.
    Legacy,
    /// Neither. The DLL is present but is not an NvFBC runtime we know.
    #[default]
    Neither,
}

/// What `NvFBC_GetStatusEx` said.
#[derive(Debug)]
pub struct Status {
    pub result: i32,
    pub capture_possible: bool,
    pub currently_capturing: bool,
    pub multi_head: bool,
    pub configurable_diffmap: bool,
    pub image_classification: bool,
    pub nvfbc_version: u32,
}

/// One `NvFBC_CreateEx` attempt.
#[derive(Debug)]
pub struct Create {
    pub result: i32,
    /// The `INvFBCToSys` instance, for [`crate::tosys`] to drive. Null on
    /// failure.
    pub object: *mut c_void,
    pub max_width: u32,
    pub max_height: u32,
}

impl Create {
    pub fn got_object(&self) -> bool {
        !self.object.is_null()
    }
}

#[derive(Debug, Default)]
pub struct Report {
    /// Which DLL name loaded, if any.
    pub dll: Option<&'static str>,
    /// A renamed original alongside the loaded DLL, which means a proxy shim is
    /// installed and the result below may be the shim's doing, not ours.
    pub proxy_shim_present: bool,
    pub generation: Generation,
    /// Legacy exports actually found, for the cases where only some are.
    pub exports: Vec<&'static str>,
    pub sdk_version: Option<u32>,
    /// `NvFBC_GetStatusEx` with no private data — what an unpatched caller sees.
    pub status_plain: Option<Status>,
    /// The same call carrying [`MAGIC`].
    pub status_keyed: Option<Status>,
    /// `NvFBC_CreateEx` without the key. Attempted first, so a success here
    /// means the key was never needed.
    pub create_plain: Option<Create>,
    /// `NvFBC_CreateEx` with the key. Only attempted if the plain one failed.
    pub create_keyed: Option<Create>,
    /// `NvFBC_Enable(ENABLE)`, only ever attempted when explicitly asked for.
    pub enable: Option<i32>,
}

/// Names the Capture SDK has shipped the 64-bit runtime under.
const CANDIDATES: [&str; 2] = ["NvFBC64.dll", "nvfbc64.dll"];

/// What a `nvidia-patch`-style proxy renames the real runtime to.
const PROXIED_ORIGINAL: &str = "NvFBC64_.dll";

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
/// `T` must be the exact signature the DLL exports under `name`. Every call
/// site below is transcribed from `nvFBC.h`.
pub(crate) unsafe fn cast_fn<T: Copy>(ptr: *const c_void) -> T {
    debug_assert_eq!(size_of::<T>(), size_of::<*const c_void>());
    // SAFETY: the caller guarantees the signature; a code pointer and a data
    // pointer are the same width on every target this runs on.
    unsafe { *(&ptr as *const *const c_void).cast::<T>() }
}

impl Status {
    fn from_raw(result: i32, raw: &NvFbcStatusEx) -> Status {
        Status {
            result,
            capture_possible: raw.flags & 0b0_0001 != 0,
            currently_capturing: raw.flags & 0b0_0010 != 0,
            // bit 2 is `bCanCreateNow`, which the header marks deprecated.
            multi_head: raw.flags & 0b00_1000 != 0,
            configurable_diffmap: raw.flags & 0b01_0000 != 0,
            image_classification: raw.flags & 0b10_0000 != 0,
            nvfbc_version: raw.nvfbc_version,
        }
    }
}

/// Ask for status once, optionally carrying the key.
fn get_status(f: PfnGetStatusEx, keyed: bool) -> Status {
    let mut params = NvFbcStatusEx {
        version: struct_version(STATUS_SIZE, 2),
        ..Default::default()
    };
    if keyed {
        params.private_data = MAGIC.as_ptr().cast();
        params.private_data_size = size_of_val(&MAGIC) as u32;
    }
    // SAFETY: `params` is a correctly laid out, fully initialised NvFBCStatusEx
    // whose `version` encodes its own size, and `MAGIC` outlives the call.
    let result = unsafe { f(&mut params) };
    Status::from_raw(result, &params)
}

/// Try to create a `NVFBC_TO_SYS` session, optionally carrying the key.
///
/// The object comes back in [`Create::object`] rather than being dropped on the
/// floor. An earlier version leaked it deliberately, because releasing means
/// calling a C++ vtable slot and the index was not known from any header on
/// hand; with the real SDK read, [`crate::tosys::ToSys`] owns and releases it.
fn try_create(f: PfnCreateEx, keyed: bool) -> Create {
    let mut params = NvFbcCreateParams {
        version: struct_version(CREATE_SIZE, 2),
        interface_type: NVFBC_TO_SYS,
        ..Default::default()
    };
    if keyed {
        params.private_data = MAGIC.as_ptr().cast();
        params.private_data_size = size_of_val(&MAGIC) as u32;
    }
    // SAFETY: `params` is a correctly laid out, fully initialised
    // NvFBCCreateParams; `pDevice`/`cudaCtx` are null, which NVFBC_TO_SYS
    // permits, and `MAGIC` outlives the call.
    let result = unsafe { f((&raw mut params).cast()) };
    Create {
        result,
        object: params.nvfbc,
        max_width: params.max_display_width,
        max_height: params.max_display_height,
    }
}

/// `attempt_enable` resets the graphics driver and needs elevation, so it is
/// never done unless asked for by name.
pub fn probe(attempt_enable: bool) -> Report {
    let mut report = Report::default();

    let Some((name, module)) = CANDIDATES.into_iter().find_map(|n| load(n).map(|m| (n, m))) else {
        return report;
    };
    report.dll = Some(name);
    report.proxy_shim_present = load(PROXIED_ORIGINAL).is_some();

    if symbol(module, "NvFBCCreateInstance").is_some() {
        report.generation = Generation::Modern;
        return report;
    }

    const LEGACY: [&str; 5] = [
        "NvFBC_CreateEx",
        "NvFBC_GetStatusEx",
        "NvFBC_Enable",
        "NvFBC_GetSDKVersion",
        "NvFBC_SetGlobalFlags",
    ];
    report.exports = LEGACY
        .into_iter()
        .filter(|n| symbol(module, n).is_some())
        .collect();
    if report.exports.is_empty() {
        return report;
    }
    report.generation = Generation::Legacy;

    if let Some(p) = symbol(module, "NvFBC_GetSDKVersion") {
        // SAFETY: signature transcribed from nvFBC.h.
        let f: PfnGetSdkVersion = unsafe { cast_fn(p) };
        let mut version = 0u32;
        // SAFETY: `version` is a valid out pointer.
        if unsafe { f(&mut version) } == 0 {
            report.sdk_version = Some(version);
        }
    }

    // Optional, explicit, and last of the pre-create steps: it toggles the
    // feature on for the whole machine and resets the display driver doing it.
    if attempt_enable
        && let Some(p) = symbol(module, "NvFBC_Enable")
    {
        // SAFETY: signature transcribed from nvFBC.h.
        let f: PfnEnable = unsafe { cast_fn(p) };
        // SAFETY: takes an NVFBC_STATE by value and returns a result code.
        report.enable = Some(unsafe { f(NVFBC_STATE_ENABLE) });
    }

    if let Some(p) = symbol(module, "NvFBC_GetStatusEx") {
        // SAFETY: signature transcribed from nvFBC.h.
        let f: PfnGetStatusEx = unsafe { cast_fn(p) };
        report.status_plain = Some(get_status(f, false));
        report.status_keyed = Some(get_status(f, true));
    }

    if let Some(p) = symbol(module, "NvFBC_SetGlobalFlags") {
        // SAFETY: signature transcribed from nvFBC.h; takes flags by value.
        let f: PfnSetGlobalFlags = unsafe { cast_fn(p) };
        // SAFETY: a documented flag value, no pointers involved.
        unsafe { f(NVFBC_GLOBAL_FLAGS_NO_INITIAL_REFRESH) };
    }

    if let Some(p) = symbol(module, "NvFBC_CreateEx") {
        // SAFETY: signature transcribed from nvFBC.h.
        let f: PfnCreateEx = unsafe { cast_fn(p) };
        let plain = try_create(f, false);
        // Only reach for the key if it is actually needed. A plain success
        // means this card was never gated and the key is a red herring.
        let needed = plain.result != 0 || !plain.got_object();
        report.create_plain = Some(plain);
        if needed {
            report.create_keyed = Some(try_create(f, true));
        }
    }

    report
}

/// Create one more keyed session, for a second capture configuration.
///
/// `NvFBCToSysSetUp` configures a session once; a run at a different pixel
/// format needs its own object rather than a second setup call on a live one.
pub fn create_session() -> Option<Create> {
    let module = CANDIDATES.into_iter().find_map(load)?;
    let p = symbol(module, "NvFBC_CreateEx")?;
    // SAFETY: signature transcribed from nvFBC.h.
    let f: PfnCreateEx = unsafe { cast_fn(p) };
    let created = try_create(f, true);
    (created.result == 0 && created.got_object()).then_some(created)
}

/// `NVFBCRESULT` names, from `nvFBC.h`.
pub fn result_name(code: i32) -> &'static str {
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
        -16 => "ERROR_INVALID_CALL",
        -17 => "ERROR_SYSTEM_ERROR",
        -18 => "ERROR_INVALID_TARGET",
        -20 => "ERROR_DYNAMIC_DISABLE",
        _ => "unknown NVFBCRESULT",
    }
}

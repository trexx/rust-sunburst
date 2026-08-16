// SPDX-License-Identifier: GPL-2.0-or-later

//! Detecting other NVENC sessions, via NVML.
//!
//! CLAUDE.md wants a startup warning when ShadowPlay, Instant Replay or OBS has
//! its own session open on our single physical encoder. The driver time-shares
//! it, and per-frame encode times get jittery in a way that looks exactly like
//! our bug — so this is the difference between a day of debugging and a line of
//! output.
//!
//! `nvmlDeviceGetEncoderStats` reports the number of active sessions on the
//! device, which is the direct answer. Loaded at runtime like NVENC: NVML is
//! part of the driver, not something to link against.

use std::ffi::{CString, c_void};

use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
use windows::core::PCSTR;

type NvmlReturn = i32;
const NVML_SUCCESS: NvmlReturn = 0;

type FnInit = unsafe extern "C" fn() -> NvmlReturn;
type FnShutdown = unsafe extern "C" fn() -> NvmlReturn;
type FnGetHandle = unsafe extern "C" fn(u32, *mut *mut c_void) -> NvmlReturn;
type FnEncoderStats = unsafe extern "C" fn(*mut c_void, *mut u32, *mut u32, *mut u32) -> NvmlReturn;

#[derive(Debug)]
pub struct EncoderStats {
    pub session_count: u32,
    pub average_fps: u32,
    pub average_latency_us: u32,
}

/// Sessions currently open on GPU 0, or why we could not tell.
pub fn encoder_sessions() -> Result<EncoderStats, String> {
    // SAFETY: a literal, NUL-terminated library name.
    let module = unsafe { LoadLibraryA(PCSTR(c"nvml.dll".as_ptr().cast())) }
        .map_err(|e| format!("nvml.dll did not load: {e}"))?;

    let sym = |name: &str| -> Result<*const c_void, String> {
        let cname = CString::new(name).map_err(|_| "bad symbol name".to_string())?;
        // SAFETY: `module` is live and `cname` is NUL-terminated.
        let p = unsafe { GetProcAddress(module, PCSTR(cname.as_ptr().cast())) }
            .ok_or_else(|| format!("{name} missing from nvml.dll"))?;
        Ok(p as *const c_void)
    };

    // SAFETY for each transmute: the export exists with this signature per the
    // NVML API. The `_v2` suffixes are the current ABI; the unsuffixed names are
    // the deprecated ones and would silently differ.
    let init: FnInit = unsafe { std::mem::transmute(sym("nvmlInit_v2")?) };
    let shutdown: FnShutdown = unsafe { std::mem::transmute(sym("nvmlShutdown")?) };
    let get_handle: FnGetHandle =
        unsafe { std::mem::transmute(sym("nvmlDeviceGetHandleByIndex_v2")?) };
    let stats: FnEncoderStats = unsafe { std::mem::transmute(sym("nvmlDeviceGetEncoderStats")?) };

    // SAFETY: no arguments, and paired with the shutdown below.
    if unsafe { init() } != NVML_SUCCESS {
        return Err("nvmlInit_v2 failed".into());
    }

    let result = (|| {
        let mut device: *mut c_void = std::ptr::null_mut();
        // SAFETY: valid out pointer; index 0 is the first GPU.
        if unsafe { get_handle(0, &mut device) } != NVML_SUCCESS {
            return Err("nvmlDeviceGetHandleByIndex_v2(0) failed".to_string());
        }

        let (mut session_count, mut average_fps, mut average_latency_us) = (0u32, 0u32, 0u32);
        // SAFETY: live device handle and three valid out pointers.
        let rc = unsafe {
            stats(
                device,
                &mut session_count,
                &mut average_fps,
                &mut average_latency_us,
            )
        };
        if rc != NVML_SUCCESS {
            return Err(format!("nvmlDeviceGetEncoderStats failed with {rc}"));
        }
        Ok(EncoderStats {
            session_count,
            average_fps,
            average_latency_us,
        })
    })();

    // SAFETY: paired with the successful init above.
    unsafe { shutdown() };
    result
}

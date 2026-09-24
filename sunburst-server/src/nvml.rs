// SPDX-License-Identifier: GPL-2.0-or-later

//! Other processes' NVENC sessions, via NVML.
//!
//! AD104 has one encoder. ShadowPlay, Instant Replay or OBS opening their own
//! session on it makes the driver time-share it, and per-frame encode times go
//! jittery in a way that looks exactly like our bug (CLAUDE.md, *Capture*
//! traps). So the server asks at startup, again before each session, and
//! whenever the web UI shows its status.
//!
//! NVML is part of the driver and loaded at runtime like NVENC, never linked.
//! `nvmlDeviceGetEncoderSessions` rather than `GetEncoderStats`, because the
//! sessions carry an owning pid and the server has to leave its own out.
//! Layout from NVIDIA's `nvml.h`: eight `unsigned int`-sized fields, `pid`
//! second. `tools/probe-windows` has the one-off count.

use std::ffi::c_void;
use std::sync::OnceLock;

use sunburst_web::host::EncoderSession;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::core::PCSTR;

use crate::proc;

type NvmlReturn = i32;
const NVML_SUCCESS: NvmlReturn = 0;
const NVML_ERROR_INSUFFICIENT_SIZE: NvmlReturn = 7;

/// `nvmlEncoderSessionInfo_t`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SessionInfo {
    session_id: u32,
    pid: u32,
    vgpu_instance: u32,
    codec_type: u32,
    h_resolution: u32,
    v_resolution: u32,
    average_fps: u32,
    average_latency: u32,
}

const _: () = assert!(size_of::<SessionInfo>() == 32);
const _: () = assert!(std::mem::offset_of!(SessionInfo, pid) == 4);

/// What `GetProcAddress` hands back, before it is given its real signature.
type RawProc = unsafe extern "system" fn() -> isize;
type FnInit = unsafe extern "C" fn() -> NvmlReturn;
type FnGetHandle = unsafe extern "C" fn(u32, *mut *mut c_void) -> NvmlReturn;
type FnSessions = unsafe extern "C" fn(*mut c_void, *mut u32, *mut SessionInfo) -> NvmlReturn;

struct Nvml {
    get_handle: FnGetHandle,
    sessions: FnSessions,
}

/// NVML, loaded and initialised once for the life of the process, or `None`
/// if the driver does not provide it. Never shut down: the process holds it
/// until it exits, which is what NVML expects of a long-lived user.
fn nvml() -> Option<&'static Nvml> {
    static NVML: OnceLock<Option<Nvml>> = OnceLock::new();
    NVML.get_or_init(|| {
        // SAFETY: a literal, NUL-terminated library name.
        let module = unsafe { LoadLibraryA(PCSTR(c"nvml.dll".as_ptr().cast())) }.ok()?;
        let sym = |name: &std::ffi::CStr| {
            // SAFETY: `module` is live and `name` is NUL-terminated.
            unsafe { GetProcAddress(module, PCSTR(name.as_ptr().cast())) }
        };
        // The `_v2` names are the current ABI; the unsuffixed ones are the
        // deprecated signatures, which would resolve and then misbehave.
        // SAFETY: each export exists with the signature NVML documents.
        let (init, get_handle, sessions) = unsafe {
            (
                std::mem::transmute::<RawProc, FnInit>(sym(c"nvmlInit_v2")?),
                std::mem::transmute::<RawProc, FnGetHandle>(sym(c"nvmlDeviceGetHandleByIndex_v2")?),
                std::mem::transmute::<RawProc, FnSessions>(sym(c"nvmlDeviceGetEncoderSessions")?),
            )
        };
        // SAFETY: takes no arguments; never paired with a shutdown (above).
        (unsafe { init() } == NVML_SUCCESS).then_some(Nvml {
            get_handle,
            sessions,
        })
    })
    .as_ref()
}

/// Encoder sessions on GPU 0 that belong to another process, or `None` if
/// NVML could not be asked.
pub fn other_encoder_sessions() -> Option<Vec<EncoderSession>> {
    let nvml = nvml()?;
    let mut device: *mut c_void = std::ptr::null_mut();
    // SAFETY: a valid out pointer; index 0 is the first (only) GPU.
    if unsafe { (nvml.get_handle)(0, &mut device) } != NVML_SUCCESS {
        return None;
    }
    let mut infos = [SessionInfo::default(); 16];
    let mut count = infos.len() as u32;
    // SAFETY: a live device handle, and `count` is the capacity of `infos`.
    let rc = unsafe { (nvml.sessions)(device, &mut count, infos.as_mut_ptr()) };
    let listed = match rc {
        NVML_SUCCESS => &infos[..(count as usize).min(infos.len())],
        // More than sixteen: report the ones that fit; any is already too many.
        NVML_ERROR_INSUFFICIENT_SIZE => &infos[..],
        _ => return None,
    };
    // SAFETY: takes no arguments and cannot fail.
    let ours = unsafe { GetCurrentProcessId() };
    let names = proc::process_names();
    Some(
        listed
            .iter()
            .filter(|s| s.pid != ours)
            .map(|s| EncoderSession {
                pid: s.pid,
                process: names.get(&s.pid).cloned().unwrap_or_default(),
                width: s.h_resolution,
                height: s.v_resolution,
            })
            .collect(),
    )
}

/// A one-line warning for a log, or `None` if there is nothing to warn about.
pub fn warning(sessions: &[EncoderSession]) -> Option<String> {
    if sessions.is_empty() {
        return None;
    }
    let list: Vec<String> = sessions
        .iter()
        .map(|s| {
            let name = if s.process.is_empty() {
                "unknown"
            } else {
                &s.process
            };
            format!("{name} (pid {}, {}x{})", s.pid, s.width, s.height)
        })
        .collect();
    Some(format!(
        "warning: another NVENC session is open: {}. It shares the GPU's one \
         encoder, so frame times will jitter. Close it, or turn off ShadowPlay / \
         Instant Replay.",
        list.join(", ")
    ))
}

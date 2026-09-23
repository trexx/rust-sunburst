// SPDX-License-Identifier: GPL-2.0-or-later

//! Real-time scheduling for the frame-path threads.
//!
//! CLAUDE.md requires the hot-path threads (the GPU thread — capture, convert,
//! encode, packetize — and the send thread) to register with MMCSS as "Games"
//! tasks and run at `THREAD_PRIORITY_TIME_CRITICAL`. MMCSS asks the scheduler to
//! treat them as multimedia work so they are not starved behind background
//! threads; TIME_CRITICAL keeps them ahead of ordinary work inside the session.
//!
//! It also owns the process's timer resolution ([`TimerResolution`]): every
//! timeout-driven wait on the frame path — the capture governor's flush
//! deadline, the send thread's idle park — is otherwise rounded up to the
//! default ~15.6 ms tick.
//!
//! The externs are hand-declared against `avrt`/`kernel32`/`winmm`, the same
//! style as the instrumentation drain's `SetThreadPriority` call, so this needs
//! no extra `windows` crate features.

use std::ffi::c_void;

#[link(name = "avrt")]
unsafe extern "system" {
    fn AvSetMmThreadCharacteristicsW(task_name: *const u16, task_index: *mut u32) -> *mut c_void;
    fn AvRevertMmThreadCharacteristics(handle: *mut c_void) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThread() -> isize;
    fn SetThreadPriority(thread: isize, priority: i32) -> i32;
    fn GetCurrentProcess() -> isize;
    fn SetProcessInformation(process: isize, class: i32, info: *const c_void, size: u32) -> i32;
}

#[link(name = "winmm")]
unsafe extern "system" {
    fn timeBeginPeriod(period_ms: u32) -> u32;
    fn timeEndPeriod(period_ms: u32) -> u32;
}

const THREAD_PRIORITY_TIME_CRITICAL: i32 = 15;

/// `PROCESS_INFORMATION_CLASS::ProcessPowerThrottling`.
const PROCESS_POWER_THROTTLING: i32 = 4;
const PROCESS_POWER_THROTTLING_CURRENT_VERSION: u32 = 1;
const PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION: u32 = 0x4;
const TIMERR_NOERROR: u32 = 0;

/// `PROCESS_POWER_THROTTLING_STATE`.
#[repr(C)]
struct PowerThrottlingState {
    version: u32,
    control_mask: u32,
    state_mask: u32,
}

/// A 1 ms system timer resolution for this process, released on drop.
///
/// Since Windows 10 2004 the resolution is per-process, and a process that does
/// not ask gets the default ~15.6 ms tick for its own timed waits. That rounds
/// every timeout on the frame path — the capture governor waking to flush a held
/// frame, the send thread's idle park — up to a sixtieth of a second, which is a
/// whole frame. Best-effort: if either call is refused the server still runs,
/// just with coarser timeouts.
pub struct TimerResolution {
    active: bool,
}

impl TimerResolution {
    /// Ask for 1 ms. Hold the returned guard for the life of the server.
    pub fn one_ms() -> TimerResolution {
        // Windows 11 stops honouring the request for a window-owning process
        // that is minimized or occluded, unless the process opts out of that
        // throttling. Older builds reject the flag; that is fine, they always
        // honour the request.
        let state = PowerThrottlingState {
            version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            control_mask: PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            state_mask: 0,
        };
        // SAFETY: `GetCurrentProcess` is a pseudo-handle that needs no close;
        // `state` is a valid PROCESS_POWER_THROTTLING_STATE of the size passed.
        unsafe {
            SetProcessInformation(
                GetCurrentProcess(),
                PROCESS_POWER_THROTTLING,
                (&state as *const PowerThrottlingState).cast(),
                size_of::<PowerThrottlingState>() as u32,
            );
        }
        // SAFETY: plain call; paired with `timeEndPeriod` on drop when accepted.
        let active = unsafe { timeBeginPeriod(1) } == TIMERR_NOERROR;
        TimerResolution { active }
    }
}

impl Drop for TimerResolution {
    fn drop(&mut self) {
        if self.active {
            // SAFETY: matches the accepted `timeBeginPeriod(1)` above.
            unsafe {
                timeEndPeriod(1);
            }
        }
    }
}

/// An MMCSS registration for the calling thread, reverted on drop.
///
/// Best-effort: MMCSS is a desktop feature, and if it declines (a stripped
/// environment, a CI runner) the thread still runs — just without the multimedia
/// scheduling class. The `TIME_CRITICAL` bump is applied either way.
pub struct RealtimeThread {
    mmcss: *mut c_void,
}

impl RealtimeThread {
    /// Register the **calling** thread as an MMCSS "Games" task at
    /// `TIME_CRITICAL`. Call once, first thing on the thread; drop it when the
    /// thread ends to revert.
    pub fn register() -> RealtimeThread {
        Self::register_task("Games")
    }

    /// Register the calling thread as an MMCSS "Pro Audio" task — the audio
    /// pipeline's class, so its 5 ms cadence is not starved behind the frame
    /// path.
    pub fn register_audio() -> RealtimeThread {
        Self::register_task("Pro Audio")
    }

    /// Register the calling thread under a named MMCSS task at `TIME_CRITICAL`.
    fn register_task(task_name: &str) -> RealtimeThread {
        // The task name as a NUL-terminated UTF-16 string.
        let task: Vec<u16> = task_name.encode_utf16().chain(std::iter::once(0)).collect();
        let mut index: u32 = 0;
        // SAFETY: `task` is NUL-terminated and outlives the call; `index` is a
        // valid out pointer. A null return means MMCSS declined, which we carry
        // as "not registered" and never revert.
        let mmcss = unsafe { AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut index) };
        // SAFETY: `GetCurrentThread` is a pseudo-handle that needs no close, and
        // `SetThreadPriority` only reads it.
        unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
        }
        RealtimeThread { mmcss }
    }
}

impl Drop for RealtimeThread {
    fn drop(&mut self) {
        if !self.mmcss.is_null() {
            // SAFETY: `mmcss` came from `AvSetMmThreadCharacteristicsW` and is
            // reverted exactly once.
            unsafe {
                AvRevertMmThreadCharacteristics(self.mmcss);
            }
        }
    }
}

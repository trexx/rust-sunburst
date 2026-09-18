// SPDX-License-Identifier: GPL-2.0-or-later

//! Real-time scheduling for the frame-path threads.
//!
//! CLAUDE.md requires the hot-path threads (the GPU thread — capture, convert,
//! encode, packetize — and the send thread) to register with MMCSS as "Games"
//! tasks and run at `THREAD_PRIORITY_TIME_CRITICAL`. MMCSS asks the scheduler to
//! treat them as multimedia work so they are not starved behind background
//! threads; TIME_CRITICAL keeps them ahead of ordinary work inside the session.
//!
//! The externs are hand-declared against `avrt`/`kernel32`, the same style as
//! the instrumentation drain's `SetThreadPriority` call, so this needs no extra
//! `windows` crate features.

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
}

const THREAD_PRIORITY_TIME_CRITICAL: i32 = 15;

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
        // "Games" as a NUL-terminated UTF-16 string.
        let task: Vec<u16> = "Games".encode_utf16().chain(std::iter::once(0)).collect();
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

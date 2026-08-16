// SPDX-License-Identifier: GPL-2.0-or-later

//! Phase 0.1's actual question: is NvFBC available on this driver?
//!
//! ROADMAP.md says to assume it is not until proven otherwise — NVIDIA
//! deprecated NvFBC on the Windows side of the Capture SDK and points Windows
//! developers at Desktop Duplication; the surviving NvFBC is the Linux one.
//!
//! The answer reorders the capture backends either way. Available makes it
//! priority 1, and worth more here than it would be on a Ti: encode time is a
//! fixed floor with one NVENC and no Split Frame Encoding, so DWM composition
//! becomes the largest remaining target. Unavailable deletes the backend from
//! the plan entirely.

use std::ffi::CString;

use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
use windows::core::PCSTR;

#[derive(Debug)]
pub enum NvfbcStatus {
    /// The DLL loaded and exports the entry point.
    Available { dll: &'static str },
    /// The DLL is present but does not export what we need — a much older
    /// Capture SDK, most likely.
    DllWithoutEntryPoint { dll: &'static str },
    /// No NvFBC DLL at all. The expected answer on a current Windows driver.
    Unavailable,
}

/// Names the Capture SDK has shipped the 64-bit runtime under.
const CANDIDATES: [&str; 2] = ["NvFBC64.dll", "nvfbc64.dll"];

pub fn probe() -> NvfbcStatus {
    for name in CANDIDATES {
        let Ok(cname) = CString::new(name) else {
            continue;
        };
        // SAFETY: `cname` is NUL-terminated and outlives the call.
        let Ok(module) = (unsafe { LoadLibraryA(PCSTR(cname.as_ptr().cast())) }) else {
            continue;
        };

        let Ok(entry) = CString::new("NvFBCCreateInstance") else {
            continue;
        };
        // SAFETY: `module` is a live handle and `entry` is NUL-terminated.
        let found = unsafe { GetProcAddress(module, PCSTR(entry.as_ptr().cast())) }.is_some();

        return if found {
            NvfbcStatus::Available { dll: name }
        } else {
            NvfbcStatus::DllWithoutEntryPoint { dll: name }
        };
    }
    NvfbcStatus::Unavailable
}

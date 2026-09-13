// SPDX-License-Identifier: GPL-2.0-or-later

//! Phase 0.1 — encoder and capture capability probe.
//!
//! ROADMAP.md asks only whether NvFBC exists. This answers that and everything
//! else the same open encode session can be asked, because the extra questions
//! cost nothing once the session is up and each one de-risks Phase 3.
//!
//! The AV1 column is the one that matters most. CLAUDE.md says not to assume
//! parity with HEVC, and the Homatics has no HEVC path behind it — so an AV1
//! encoder without `SUPPORT_REF_PIC_INVALIDATION` means NACK recovery on that
//! box degrades to `RequestIdr`, and Phase 4 is shaped differently. Better known
//! now than then.
//!
//! Run on the 4070 box and paste the output into HARDWARE_TESTING.md §1.
//!
//! ```text
//! probe-windows                  # NvFBC via ToCuda, the GPU-resident path
//! probe-windows --latency        # present -> capture, DDA vs WGC vs NvFBC
//! probe-windows --tosys          # also the sysmem path, which copies every frame
//! probe-windows --enable-nvfbc   # NvFBC_Enable: needs elevation, resets the driver
//! ```
//!
//! `--latency` is the one that prices CLAUDE.md's DWM composition line. It opens
//! a small topmost window of its own, so leave the screen alone while it runs --
//! covering that window stops the signal reaching any backend.
//!
//! Run it with something animating full-screen. An idle desktop has already
//! produced two wrong conclusions in this investigation, so the capture section
//! now refuses to draw one rather than repeat that.

#[cfg(windows)]
mod capture;
#[cfg(windows)]
mod cuda;
#[cfg(windows)]
mod dda;
#[cfg(windows)]
mod latency;
#[cfg(windows)]
mod nvenc;
#[cfg(windows)]
mod nvfbc;
#[cfg(windows)]
mod nvml;
#[cfg(windows)]
mod presenter;
#[cfg(windows)]
mod probe;
#[cfg(windows)]
mod readback;
#[cfg(windows)]
mod tocuda;
#[cfg(windows)]
mod tosys;
#[cfg(windows)]
mod watch;

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    // `NvFBC_Enable` switches the feature on machine-wide and resets the display
    // driver doing it, which on a box someone is watching looks indistinguishable
    // from a crash. A probe does not get to do that unasked.
    let attempt_enable = std::env::args().any(|a| a == "--enable-nvfbc");
    if std::env::args().any(|a| a == "--latency") {
        latency::run();
        return std::process::ExitCode::SUCCESS;
    }
    probe::run(attempt_enable)
}

// The crate stays a workspace member on Linux so it is type-checked in the same
// pass as everything else. `cargo xwin check --target x86_64-pc-windows-msvc`
// is what actually checks the code above.
#[cfg(not(windows))]
fn main() {
    eprintln!("probe-windows runs on the server, against a real NVENC.");
    eprintln!("Cross-check it from here with:");
    eprintln!("  cargo xwin check --target x86_64-pc-windows-msvc -p probe-windows");
}

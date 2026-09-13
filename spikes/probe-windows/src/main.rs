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

#[cfg(windows)]
mod dda;
#[cfg(windows)]
mod nvenc;
#[cfg(windows)]
mod nvfbc;
#[cfg(windows)]
mod nvml;
#[cfg(windows)]
mod probe;
#[cfg(windows)]
mod tosys;

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    // `NvFBC_Enable` switches the feature on machine-wide and resets the display
    // driver doing it, which on a box someone is watching looks indistinguishable
    // from a crash. A probe does not get to do that unasked.
    let attempt_enable = std::env::args().any(|a| a == "--enable-nvfbc");
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

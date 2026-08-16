// SPDX-License-Identifier: GPL-2.0-or-later

//! The server binary.
//!
//! Runs in the interactive session — there is no service. See
//! `win_host.rs` for why, and for what that costs.

#[cfg(windows)]
#[tokio::main]
async fn main() -> std::process::ExitCode {
    match sunburst_server::run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sunburst: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

// The crate stays a workspace member on Linux so it is type-checked in the same
// pass as everything else. `cargo xwin check --target x86_64-pc-windows-msvc` is
// what actually checks the code above.
#[cfg(not(windows))]
fn main() {
    eprintln!("sunburst-server runs on Windows: capture and SendInput both need");
    eprintln!("the interactive session. Cross-check it from here with:");
    eprintln!("  cargo xwin check --target x86_64-pc-windows-msvc -p sunburst-server");
}

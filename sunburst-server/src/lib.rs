// SPDX-License-Identifier: GPL-2.0-or-later
#![cfg(windows)]

//! Session orchestration. tokio lives here and only here.
//!
//! Windows-only. On any other host this crate compiles to nothing, so
//! `cargo check` stays green on the Linux development machine while
//! `cargo xwin check --target x86_64-pc-windows-msvc` checks the real thing.
//! Neither command lies about the other.

pub mod win_host;

pub use win_host::WindowsHost;

use std::net::SocketAddr;
use std::sync::Arc;

use sunburst_web::api::AppState;
use sunburst_web::{Store, http};

/// Load state, bind, and serve until the process ends.
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let store = Store::open_default()?;
    let config_dir = store.dir().to_path_buf();

    // The drain is not started here yet: nothing is instrumented until Phase 2
    // gives it something to measure. `/api/metrics` answers 503 until it is,
    // which is the honest answer — an empty table would read as "everything is
    // fast".
    let host = Arc::new(WindowsHost::new(None));
    let state = Arc::new(AppState::load(store, host)?);

    let web = state.web_config();
    let addr = SocketAddr::new(web.bind, web.port);
    let listener = http::bind(addr).await?;

    println!("sunburst — configuration in {}", config_dir.display());
    println!("  http://{addr}/");
    // Printed rather than left in the file, so the token can be found without
    // going looking for it. This is the control plane, not a frame path, and it
    // happens once at startup.
    println!("  token: {}", state.token());
    if web.is_lan_exposed() {
        println!("  reachable from the network — the token is the only gate");
    }

    http::serve(listener, state).await;
    Ok(())
}

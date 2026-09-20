// SPDX-License-Identifier: GPL-2.0-or-later
#![cfg(windows)]

//! Session orchestration. tokio lives here and only here.
//!
//! Windows-only. On any other host this crate compiles to nothing, so
//! `cargo check` stays green on the Linux development machine while
//! `cargo xwin check --target x86_64-pc-windows-msvc` checks the real thing.
//! Neither command lies about the other.

pub mod audio_pipeline;
pub mod cursor;
pub mod display;
pub mod mic_pipeline;
pub mod pipeline;
pub mod realtime;
pub mod session;
pub mod win_host;

pub use pipeline::{CodecHeaders, Pipeline, PipelineParams};
pub use session::{SessionManager, Sessions};
pub use win_host::WindowsHost;

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use sunburst_core::instr;
use sunburst_input::Injector;
use sunburst_net::Endpoint;
use sunburst_web::api::AppState;
use sunburst_web::{Store, WebHandler, http};

/// Load state, bind, and serve until the process ends.
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let store = Store::open_default()?;
    let config_dir = store.dir().to_path_buf();

    // The frame-path pipeline records into the instrumentation ring, so start
    // the drain and hand the web host a handle: `/api/metrics` now has real
    // stage timings to report rather than the honest 503 it gave when nothing
    // was measured.
    let drain = instr::spawn(Duration::from_millis(100), Duration::from_secs(10));
    // The live-session registry the web UI reads and the session manager writes.
    let sessions = Sessions::new();
    let host = Arc::new(WindowsHost::new(Some(drain), Arc::clone(&sessions)));
    let state = Arc::new(AppState::load(store, host)?);

    let web = state.web_config();
    let addr = SocketAddr::new(web.bind, web.port);
    let listener = http::bind(addr).await?;

    // The control channel and input, on their own threads with blocking
    // sockets. No tokio below this line: CLAUDE.md keeps the input path off an
    // async runtime, and the injector thread has to be the one attached to the
    // desktop it injects into.
    let stream_port = state.stream_port();
    let stream_addr = SocketAddr::new(web.bind, stream_port);
    // The vendored HIDMaestro driver's INF, when the box is provisioned with it
    // (set `SUNBURST_DRIVER_INF` to the extracted `hidmaestro.inf`). Absent, the
    // injector maps sections but does not create device nodes.
    let driver_inf = std::env::var_os("SUNBURST_DRIVER_INF").map(std::path::PathBuf::from);
    let injector = Injector::start(driver_inf)?;

    // Bind the shared UDP socket here, so the video send path can hold a clone
    // of the very same socket while the endpoint owns it for receive.
    let socket = UdpSocket::bind(stream_addr)?;
    let stream_socket = socket.try_clone()?;
    let session_mgr = SessionManager::new(stream_socket, sessions);

    let handler = WebHandler::new(Arc::clone(&state), injector, session_mgr);
    let mut endpoint = Endpoint::from_socket(socket, handler)?;
    // A paired client's `Hello` now starts the stream; the old env gate is gone.

    let stop = Box::leak(Box::new(AtomicBool::new(false)));
    std::thread::spawn(move || {
        let _ = endpoint.run(stop);
    });

    println!("sunburst — configuration in {}", config_dir.display());
    println!("  http://{addr}/");
    println!("  udp  {stream_addr}");
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

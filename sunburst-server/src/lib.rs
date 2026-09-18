// SPDX-License-Identifier: GPL-2.0-or-later
#![cfg(windows)]

//! Session orchestration. tokio lives here and only here.
//!
//! Windows-only. On any other host this crate compiles to nothing, so
//! `cargo check` stays green on the Linux development machine while
//! `cargo xwin check --target x86_64-pc-windows-msvc` checks the real thing.
//! Neither command lies about the other.

pub mod pipeline;
pub mod realtime;
pub mod win_host;

pub use pipeline::{CodecHeaders, Pipeline, PipelineConfig};
pub use win_host::WindowsHost;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use sunburst_core::instr;
use sunburst_encode::encoder::Codec;
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
    let host = Arc::new(WindowsHost::new(Some(drain)));
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
    let mut endpoint = Endpoint::bind(stream_addr, WebHandler::new(Arc::clone(&state), injector))?;

    // Bring-up hook for the frame path. There is no session-negotiation signal
    // yet — the client-connected event and codec negotiation land with
    // `SessionConfig` (see the endpoint docs) — so streaming is gated on
    // `SUNBURST_STREAM_TO=host:port` rather than started automatically, the same
    // shape as `SUNBURST_DRIVER_INF` above. It streams to a stub receiver so the
    // capture→convert→encode→packetize→send p99 can be recorded on the box.
    // Held to the end of `run` so its threads live as long as the server.
    let _pipeline = start_stream_if_configured(&endpoint);

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

/// Start the frame-path pipeline when `SUNBURST_STREAM_TO` names a target.
///
/// Bring-up only (see the call site in [`run`]): there is no client-connected /
/// codec-negotiation signal yet, so this is env-gated rather than automatic. It
/// clones the endpoint's socket so video leaves the one shared port, picks the
/// codec from `SUNBURST_STREAM_CODEC` (`av1`, else HEVC), and forwards the
/// sequence headers to the target for a stub receiver. Returns the running
/// [`Pipeline`], or `None` when unset or the stream fails to start.
fn start_stream_if_configured<H: sunburst_net::ControlHandler>(
    endpoint: &Endpoint<H>,
) -> Option<Pipeline> {
    let target = std::env::var_os("SUNBURST_STREAM_TO")?;
    let client: SocketAddr = match target.to_string_lossy().parse() {
        Ok(addr) => addr,
        Err(e) => {
            eprintln!("SUNBURST_STREAM_TO is not host:port: {e}");
            return None;
        }
    };
    let codec = match std::env::var("SUNBURST_STREAM_CODEC").as_deref() {
        Ok("av1") => Codec::Av1,
        _ => Codec::Hevc,
    };
    let cfg = PipelineConfig::new(codec, true);

    let video_sock = endpoint.try_clone_socket().ok()?;
    let header_sock = endpoint.try_clone_socket().ok()?;
    let (headers_tx, headers_rx) = std::sync::mpsc::channel::<CodecHeaders>();

    // Deliver sequence headers. For bring-up we log them and forward the raw
    // bytes to the target so a stub receiver has the codec config before the
    // first frame; reliable delivery to a negotiated client lands with
    // SessionConfig.
    std::thread::spawn(move || {
        while let Ok(h) = headers_rx.recv() {
            println!(
                "  stream: {:?} sequence headers, {} bytes",
                h.codec,
                h.sequence.len()
            );
            let _ = header_sock.send_to(&h.sequence, client);
        }
    });

    match Pipeline::spawn(video_sock, client, cfg, headers_tx) {
        Ok(pipeline) => {
            println!("  streaming to {client} ({codec:?})");
            Some(pipeline)
        }
        Err(e) => {
            eprintln!("  stream start failed: {e}");
            None
        }
    }
}

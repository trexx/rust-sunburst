// SPDX-License-Identifier: GPL-2.0-or-later

//! Run the management API against a fake host, on any platform.
//!
//! This is what makes the frontend developable on the Linux machine. Without it
//! the UI could only be exercised on the 5070 box, which would waste the whole
//! reason [`sunburst_web::host::Host`] is a trait.
//!
//! ```text
//! cargo run -p sunburst-web --example serve
//! cd web && npm run dev      # proxies /api to 127.0.0.1:47810
//! ```
//!
//! State goes to a scratch directory, not the real one, so experimenting here
//! cannot revoke a pairing that matters.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use sunburst_net::Endpoint;
use sunburst_web::api::AppState;
use sunburst_web::config::Config;
use sunburst_web::host::{Fake, Host, SessionSummary};
use sunburst_web::{Store, WebHandler, http};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::var("SUNBURST_CONFIG_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("sunburst-dev"));
    std::fs::create_dir_all(&dir)?;

    let store = Store::at(&dir);

    // Point at the built frontend unless the caller says otherwise. With
    // `npm run dev` this is unused — Vite serves the UI and proxies the API here.
    let mut config = store.load_config()?;
    if config.web.assets_dir == Config::default().web.assets_dir {
        config.web.assets_dir = std::path::PathBuf::from("web/dist");
        store.save_config(&config)?;
    }

    // A session and some metrics, so the panels have something to render. Real
    // ones do not exist until Phase 4.
    let fake = Fake::with_sessions(vec![SessionSummary {
        id: 1,
        client_id: 1,
        client_name: "Living room".into(),
        codec: "hevc".into(),
        width: 3840,
        height: 2160,
        fps: 60,
        bitrate_kbps: 120_000,
        started_at: now(),
        app_id: None,
    }]);
    fake.set_metrics(Some(demo_report()));

    let state = Arc::new(AppState::load(store, Arc::new(fake) as Arc<dyn Host>)?);
    let web = state.web_config();
    let addr = SocketAddr::new(web.bind, web.port);
    let listener = http::bind(addr).await?;

    // The control channel, on its own thread with a blocking socket. This is
    // what makes the whole pairing path drivable from `tools/fakeclient` without
    // a Windows box or an Android device.
    let stream_addr = SocketAddr::new(web.bind, config.stream.port);
    let mut endpoint = Endpoint::bind(
        stream_addr,
        WebHandler::new(Arc::clone(&state), sunburst_web::NoInput),
    )?;
    let stop = Box::leak(Box::new(AtomicBool::new(false)));
    std::thread::spawn(move || {
        let _ = endpoint.run(stop);
    });

    println!("sunburst-web (fake host) — state in {}", dir.display());
    println!("  http://{addr}/");
    println!("  udp  {stream_addr}");
    println!("  token: {}", state.token());

    http::serve(listener, state).await;
    Ok(())
}

fn demo_report() -> sunburst_core::instr::Report {
    use sunburst_core::instr::{Report, Stage, StageStats};

    let stage = |stage: Stage, p50: u64, p99: u64| StageStats {
        stage,
        count: 3600,
        p50_ns: p50,
        p95_ns: (p50 + p99) / 2,
        p99_ns: p99,
        max_ns: p99 * 2,
    };

    // Roughly CLAUDE.md's latency budget, so the table looks like something real
    // rather than like round numbers.
    Report {
        stages: vec![
            stage(Stage::ColorConvert, 900_000, 1_500_000),
            stage(Stage::EncodeSubmit, 200_000, 400_000),
            stage(Stage::EncodeUnitOut, 6_100_000, 9_100_000),
            stage(Stage::Packetize, 180_000, 320_000),
            stage(Stage::Send, 90_000, 210_000),
        ],
        dropped_samples: 0,
        dropped_by_thread: Vec::new(),
        unregistered_threads: 0,
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

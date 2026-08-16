// SPDX-License-Identifier: GPL-2.0-or-later

//! The hyper layer, over a real socket.
//!
//! `api::dispatch` is tested directly elsewhere; what this covers is everything
//! between the wire and it — header extraction, body collection and its limit,
//! status mapping, and static asset serving. None of that is exercised by a
//! test that calls `dispatch`.
//!
//! The client is hand-written rather than pulled in: these are fixed requests to
//! a known server, and an HTTP client crate for that would be a dependency
//! bought for six tests.

use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use sunburst_web::api::AppState;
use sunburst_web::config::Config;
use sunburst_web::host::{Fake, Host};
use sunburst_web::{Store, http};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

struct Temp(PathBuf);

impl Temp {
    fn new(tag: &str) -> Temp {
        let mut p = std::env::temp_dir();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        p.push(format!("sunburst-http-{tag}-{unique}"));
        fs::create_dir_all(&p).expect("create temp dir");
        Temp(p)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Server {
    addr: SocketAddr,
    token: String,
    _dir: Temp,
}

/// Start a server on an ephemeral port with an assets directory.
async fn start(tag: &str) -> Server {
    let dir = Temp::new(tag);

    let assets = dir.0.join("assets");
    fs::create_dir_all(&assets).expect("assets dir");
    fs::write(assets.join("index.html"), "<h1>sunburst</h1>").expect("index");
    fs::write(assets.join("app.js"), "export const x = 1;").expect("js");

    let mut config = Config::default();
    config.web.assets_dir = assets;
    // Port 0 is not usable here: the config is what the server binds from, so
    // the listener is created directly below instead.
    Store::at(&dir.0).save_config(&config).expect("save config");

    let host = Arc::new(Fake::new()) as Arc<dyn Host>;
    let state = Arc::new(AppState::load(Store::at(&dir.0), host).expect("state"));
    let token = state.token();

    let listener = http::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(http::serve(listener, state));
    Server {
        addr,
        token,
        _dir: dir,
    }
}

/// Send a raw request and return (status, body).
async fn send(addr: SocketAddr, request: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    stream.flush().await.expect("flush");

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read response");
    let text = String::from_utf8_lossy(&raw).into_owned();

    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line in:\n{text}"));
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

fn get(path: &str, token: Option<&str>) -> String {
    let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{auth}\r\n")
}

#[tokio::test]
async fn an_authorised_request_is_served() {
    let server = start("ok").await;
    let (status, body) = send(server.addr, &get("/api/status", Some(&server.token))).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"pid\""), "{body}");
}

#[tokio::test]
async fn the_authorization_header_actually_reaches_dispatch() {
    // The header is extracted in the hyper layer, so a mistake there would make
    // every request look anonymous while `dispatch`'s own tests still passed.
    let server = start("auth").await;

    let (unauthorised, _) = send(server.addr, &get("/api/status", None)).await;
    assert_eq!(unauthorised, 401);

    let (wrong, _) = send(server.addr, &get("/api/status", Some("nope"))).await;
    assert_eq!(wrong, 401);
}

#[tokio::test]
async fn a_json_body_round_trips_over_the_wire() {
    let server = start("body").await;
    let payload = r#"{"name":"Big Picture","exe":"steam://open/bigpicture"}"#;
    let request = format!(
        "POST /api/apps HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
         Authorization: Bearer {}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{payload}",
        server.token,
        payload.len()
    );

    let (status, body) = send(server.addr, &request).await;
    assert_eq!(status, 201, "{body}");
    assert!(body.contains("Big Picture"), "{body}");
}

#[tokio::test]
async fn an_oversized_body_is_refused() {
    // The limit has to stop the read rather than measure it afterwards, so this
    // sends more than the cap and expects a refusal rather than a buffered
    // megabyte.
    let server = start("toobig").await;
    let payload = "x".repeat(2 * 1024 * 1024);
    let request = format!(
        "POST /api/apps HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
         Authorization: Bearer {}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{payload}",
        server.token,
        payload.len()
    );

    let (status, _) = send(server.addr, &request).await;
    assert_eq!(status, 413);
}

#[tokio::test]
async fn static_assets_are_served_with_a_useful_content_type() {
    let server = start("assets").await;

    let mut stream = TcpStream::connect(server.addr).await.expect("connect");
    stream
        .write_all(get("/app.js", None).as_bytes())
        .await
        .expect("write");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read");
    let text = String::from_utf8_lossy(&raw);

    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert!(
        text.contains("text/javascript"),
        "a JS asset served as something else breaks the module load: {text}"
    );
}

#[tokio::test]
async fn the_index_is_served_at_the_root_without_a_token() {
    // Static assets are deliberately not behind the token: the page has to load
    // before anyone can type one in.
    let server = start("index").await;
    let (status, body) = send(server.addr, &get("/", None)).await;
    assert_eq!(status, 200);
    assert!(body.contains("sunburst"), "{body}");
}

#[tokio::test]
async fn an_unknown_route_falls_back_to_the_index_but_a_missing_asset_does_not() {
    let server = start("spa").await;

    // Looks like a client-side route.
    let (status, body) = send(server.addr, &get("/settings", None)).await;
    assert_eq!(status, 200);
    assert!(body.contains("sunburst"), "{body}");

    // Looks like an asset. Returning HTML here would surface as a baffling
    // syntax error in the browser rather than a 404.
    let (status, _) = send(server.addr, &get("/missing.js", None)).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn path_traversal_is_refused() {
    let server = start("traversal").await;
    // Sent raw so the escape reaches the server rather than being normalised by
    // a client library on the way out.
    let (status, _) = send(server.addr, &get("/../../etc/passwd", None)).await;
    assert_ne!(status, 200, "traversal returned content");
}

#[tokio::test]
async fn an_unknown_api_route_is_a_404_with_json() {
    let server = start("api404").await;
    let (status, body) = send(server.addr, &get("/api/nope", Some(&server.token))).await;
    assert_eq!(status, 404);
    assert!(body.contains("\"error\""), "{body}");
}

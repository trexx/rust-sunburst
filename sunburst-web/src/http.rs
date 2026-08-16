// SPDX-License-Identifier: GPL-2.0-or-later

//! The hyper layer. The only part of this crate that knows about HTTP types.
//!
//! Everything it does is adapt: collect the body, hand the request to
//! [`crate::api::dispatch`], and turn the answer back into a response. Static
//! assets are served straight from disk.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use http_body_util::LengthLimitError;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use crate::api::{self, ApiRequest, ApiResponse, AppState};

/// Largest request body accepted.
///
/// An admin API's biggest body is an app entry. Anything approaching a megabyte
/// is a mistake or an attempt to exhaust memory, and refusing it costs nothing.
const MAX_BODY: usize = 1024 * 1024;

/// How long to keep retrying the bind.
///
/// A restart spawns the replacement before the old process has exited, so the
/// port is still held for a moment. Without this the new process loses the race
/// and dies, taking the web UI with it — after the user asked for a restart.
const BIND_RETRY_SECS: u64 = 10;

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("could not bind {addr} after {secs}s: {source}")]
    Bind {
        addr: SocketAddr,
        secs: u64,
        #[source]
        source: std::io::Error,
    },
}

/// Bind, retrying while the previous process lets go of the port.
pub async fn bind(addr: SocketAddr) -> Result<TcpListener, ServeError> {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(BIND_RETRY_SECS);
    loop {
        match TcpListener::bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(source) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(ServeError::Bind {
                        addr,
                        secs: BIND_RETRY_SECS,
                        source,
                    });
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
            }
        }
    }
}

/// Accept connections until the process ends.
pub async fn serve(listener: TcpListener, state: Arc<AppState>) {
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(pair) => pair,
            // A failed accept is not a reason to stop answering: the usual cause
            // is a client that vanished between the SYN and the accept.
            Err(_) => continue,
        };

        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let service = service_fn(move |req| handle(Arc::clone(&state), req));
            let _ = http1::Builder::new().serve_connection(io, service).await;
        });
    }
}

async fn handle(
    state: Arc<AppState>,
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    let authorization = req
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    if !path.starts_with("/api/") {
        return Ok(static_asset(&state, &path));
    }

    let body = match collect_body(req).await {
        Ok(bytes) => bytes,
        Err(response) => return Ok(response),
    };

    let api_req = ApiRequest {
        method,
        path,
        authorization,
        body,
    };
    Ok(to_response(api::dispatch(&state, &api_req, unix_now())))
}

async fn collect_body(req: Request<Incoming>) -> Result<Vec<u8>, Response<Full<Bytes>>> {
    // `Limited` stops reading once the cap is passed. Checking the size after
    // collecting would mean having already buffered whatever was sent, which is
    // the memory exhaustion the limit exists to prevent — and `Content-Length`
    // cannot be trusted for an early rejection either, since it can lie.
    match Limited::new(req.into_body(), MAX_BODY).collect().await {
        Ok(collected) => Ok(collected.to_bytes().to_vec()),
        Err(e) if e.downcast_ref::<LengthLimitError>().is_some() => {
            Err(json_error(413, "request body too large"))
        }
        Err(_) => Err(json_error(400, "could not read request body")),
    }
}

fn json_error(status: u16, message: &str) -> Response<Full<Bytes>> {
    to_response(ApiResponse {
        status,
        content_type: "application/json",
        body: format!(r#"{{"error":"{message}"}}"#).into_bytes(),
    })
}

fn to_response(api: ApiResponse) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::from_u16(api.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header(hyper::header::CONTENT_TYPE, api.content_type)
        // Nothing here is cacheable: it is all live state.
        .header(hyper::header::CACHE_CONTROL, "no-store")
        .body(Full::new(Bytes::from(api.body)))
        .expect("response builder inputs are all valid")
}

fn static_asset(state: &AppState, path: &str) -> Response<Full<Bytes>> {
    let root = state.web_config().assets_dir;
    let relative = if path == "/" {
        "index.html"
    } else {
        path.trim_start_matches('/')
    };

    let Some(full) = safe_join(&root, relative) else {
        return plain(StatusCode::FORBIDDEN, "forbidden");
    };

    match std::fs::read(&full) {
        Ok(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header(hyper::header::CONTENT_TYPE, content_type(&full))
            .body(Full::new(Bytes::from(bytes)))
            .expect("response builder inputs are all valid"),
        // A single-page app serves index.html for unknown paths so a deep link
        // works, but only for something that looks like a route rather than a
        // missing asset — otherwise a typo'd script URL returns HTML and the
        // browser reports a baffling syntax error.
        Err(_) if !relative.contains('.') => match std::fs::read(root.join("index.html")) {
            Ok(bytes) => Response::builder()
                .status(StatusCode::OK)
                .header(hyper::header::CONTENT_TYPE, "text/html; charset=utf-8")
                .body(Full::new(Bytes::from(bytes)))
                .expect("response builder inputs are all valid"),
            Err(_) => plain(StatusCode::NOT_FOUND, NO_ASSETS),
        },
        Err(_) => plain(StatusCode::NOT_FOUND, "not found"),
    }
}

const NO_ASSETS: &str = "The web UI is not built. Run `npm run build` in web/, \
or point web.assets_dir at the built output.";

/// Join, refusing anything that could escape the assets directory.
///
/// Rejects `..` and absolute components outright rather than canonicalising and
/// comparing, because the file may not exist yet and canonicalisation of a
/// missing path is not portable.
fn safe_join(root: &Path, relative: &str) -> Option<PathBuf> {
    let candidate = Path::new(relative);
    for component in candidate.components() {
        match component {
            Component::Normal(_) => {}
            // Everything else is either an escape or an absolute path.
            _ => return None,
        }
    }
    Some(root.join(candidate))
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn plain(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(message.to_string())))
        .expect("response builder inputs are all valid")
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_attempts_are_refused() {
        let root = Path::new("/srv/web");
        assert!(safe_join(root, "../../etc/passwd").is_none());
        assert!(safe_join(root, "a/../../b").is_none());
        assert!(safe_join(root, "/etc/passwd").is_none());
        assert!(
            safe_join(root, "./index.html").is_none(),
            "curdir is not normal"
        );
    }

    #[test]
    fn ordinary_paths_join() {
        let root = Path::new("/srv/web");
        assert_eq!(
            safe_join(root, "assets/app.js"),
            Some(PathBuf::from("/srv/web/assets/app.js"))
        );
        assert_eq!(
            safe_join(root, "index.html"),
            Some(PathBuf::from("/srv/web/index.html"))
        );
    }

    #[test]
    fn content_types_cover_what_vite_emits() {
        assert_eq!(
            content_type(Path::new("a.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            content_type(Path::new("a.js")),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type(Path::new("a.css")), "text/css; charset=utf-8");
        assert_eq!(content_type(Path::new("a.woff2")), "font/woff2");
        assert_eq!(
            content_type(Path::new("a.unknown")),
            "application/octet-stream"
        );
    }

    #[test]
    fn api_responses_carry_their_status_and_type() {
        let response = to_response(ApiResponse {
            status: 404,
            content_type: "application/json",
            body: b"{}".to_vec(),
        });
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response
                .headers()
                .get(hyper::header::CACHE_CONTROL)
                .unwrap(),
            "no-store"
        );
    }

    #[test]
    fn an_impossible_status_does_not_panic() {
        // `ApiResponse::status` is a u16 and nothing stops a future handler from
        // getting it wrong; a panic here would take down the connection task.
        let response = to_response(ApiResponse {
            status: 9999,
            content_type: "application/json",
            body: Vec::new(),
        });
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}

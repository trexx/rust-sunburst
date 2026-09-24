// SPDX-License-Identifier: GPL-2.0-or-later

//! The management web UI: clients, sessions, configuration, autostart, and the
//! app catalogue the client launches from.
//!
//! # Not a frame path
//!
//! This is the control plane. It reads instrumentation snapshots and issues
//! commands; it never sits between a captured frame and the wire, which is why
//! `tokio` and `serde` are allowed here and nowhere near `sunburst-core`'s hot
//! path.
//!
//! # Cross-platform on purpose
//!
//! Everything Windows-specific lives behind [`host::Host`], so routing, auth,
//! config, pairing and the catalogue can all be exercised on the Linux
//! development machine. That is the whole reason the trait exists — without it
//! none of this could be tested without the server hardware.
//!
//! # Security posture
//!
//! LAN-only, bearer token, no TLS — consistent with CLAUDE.md already declining
//! encryption for video on the same network. The token is a shared-secret gate,
//! not confidentiality on the wire. A LAN bind without a token is refused at
//! load time by [`config::Config::validate`], so "never an open relay" holds by
//! construction rather than by remembering.

pub mod api;
pub mod apptrack;
pub mod auth;
pub mod client;
pub mod config;
pub mod control;
pub mod host;
pub mod http;
pub mod metrics;
pub mod pairing;
pub mod random;
pub mod store;

pub use api::{ApiRequest, ApiResponse, AppState, dispatch};
pub use config::{AppEntry, Config};
pub use control::{InputSink, NoInput, NoStream, StreamControl, WebHandler};
pub use host::{Host, HostError};
pub use store::{Store, StoreError};

// SPDX-License-Identifier: GPL-2.0-or-later

//! UDP transport: packetization, pacing, NACK and rate control.
//!
//! Cross-platform, unlike the other server crates — the client needs the
//! receive and depacketize halves. Windows-only send offload (USO/URO) is
//! `cfg`-gated within, not at the crate boundary.
//!
//! # No async runtime
//!
//! `std::net::UdpSocket` on dedicated threads, per CLAUDE.md. tokio is control
//! plane only and lives in `sunburst-server` and `sunburst-web`; nothing here
//! touches it, including the control channel, because it shares one socket with
//! video.

pub mod endpoint;
pub mod handler;
pub mod reliable;

pub use endpoint::{ClientEndpoint, Endpoint, MAX_CONTROL_PAYLOAD};
pub use handler::{ControlHandler, InputSink, NoInput, Outbound, Recording};
pub use reliable::{Reliable, ReliableError};

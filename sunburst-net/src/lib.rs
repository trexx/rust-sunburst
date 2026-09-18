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
pub mod owd;
pub mod rate;
pub mod refinval;
pub mod reliable;
pub mod retransmit;
pub mod send;
pub mod spsc;
pub mod video;

pub use endpoint::{ClientEndpoint, Endpoint, MAX_CONTROL_PAYLOAD};
pub use handler::{
    ControlHandler, InputSink, NoInput, NoStream, Outbound, Recording, StreamControl,
};
pub use owd::{OwdGradient, TickUnwrap};
pub use rate::{Bounds, RateController};
pub use refinval::{Av1RefState, HevcRefState, IntraRefresh, Recovery, RefState};
pub use reliable::{Reliable, ReliableError};
pub use retransmit::RetransmitCache;
pub use send::{Batch, MAX_BATCH, Pacer, PlainSender, Sender};
pub use spsc::{Consumer, Producer, SLOT_BYTES, packet_ring};
pub use video::{Accept, FrameRef, JitterBuffer, Packetizer, Reassembler, Released};

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

pub mod audio;
pub mod cursor;
pub mod endpoint;
pub mod governor;
pub mod handler;
pub mod owd;
pub mod rate;
pub mod refinval;
pub mod reliable;
pub mod retransmit;
pub mod send;
pub mod spsc;
pub mod video;

pub use audio::{
    MAX_AUDIO_PACKET, encode_audio_in_packet, encode_audio_packet, parse_audio_in_packet,
    parse_audio_packet,
};
pub use endpoint::{ClientEndpoint, Endpoint, Inbound, MAX_CONTROL_PAYLOAD};
pub use governor::{Admit, FrameGovernor, client_interval_ns, encoder_fps};
pub use handler::{
    CaptureBackend, ControlHandler, InputSettings, InputSink, NoInput, NoStream, Outbound,
    Recording, SessionSettings, StreamControl,
};
pub use owd::{OwdGradient, TickUnwrap};
pub use rate::{BitrateAsk, Bounds, RateController, codec_ceiling_kbps, session_bitrate};
pub use refinval::{Av1RefState, H264RefState, HevcRefState, IntraRefresh, Recovery, RefState};
pub use reliable::{Reliable, ReliableError};
pub use retransmit::RetransmitCache;
pub use send::{Batch, MAX_BATCH, Pacer, PlainSender, Sender, video_pace_bps};
pub use spsc::{Consumer, Producer, SLOT_BYTES, packet_ring};
pub use video::{Accept, FrameRef, JitterBuffer, Packetizer, Reassembler, Released};

// SPDX-License-Identifier: GPL-2.0-or-later

//! WASAPI loopback capture and Opus.
//!
//! Unlike `sunburst-capture`/`-encode`, this crate is **not** `#![cfg(windows)]`
//! at the root. The Opus codec wrapper ([`codec`]) and the PCM plumbing
//! ([`pcm`]) are pure Rust — the codec is [`unsafe_libopus`], transpiled from
//! the reference C — so they compile and are tested on the Linux host, and the
//! Android client links [`codec::OpusDecoder`] the same as the server links
//! [`codec::OpusEncoder`]. Only the WASAPI capture ([`device`], [`capture`]) is
//! `#[cfg(windows)]`, the same host-testable/gated split `sunburst-input` uses.
//!
//! On Windows the server drives [`capture::LoopbackCapture`] on a 5 ms cadence,
//! converts each packet to stereo i16 ([`pcm`]), and encodes it
//! ([`codec::OpusEncoder`]); the transport and A/V-sync live in `sunburst-net`
//! and `sunburst-server`.

pub mod codec;
pub mod pcm;

#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod device;
#[cfg(windows)]
pub mod render;

#[cfg(windows)]
pub use capture::{CaptureFormat, LoopbackCapture};
#[cfg(windows)]
pub use render::RenderPlayback;

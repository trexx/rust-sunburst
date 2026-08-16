// SPDX-License-Identifier: GPL-2.0-or-later

//! The wire format, transcribed from PROTOCOL.md.
//!
//! Both ends are ours, so there is no compatibility constraint — which is
//! exactly why the format is pinned in a document and mirrored here rather than
//! being invented afresh by whichever side is being worked on.
//!
//! Everything encodes into caller-supplied buffers. Nothing on the frame or
//! input path allocates.
//!
//! # Receiving an authenticated packet
//!
//! The order matters and is not interchangeable:
//!
//! 1. [`header::Header::decode`] — reject unknown types and short buffers.
//! 2. [`auth::SessionKey::verify_packet`] — reject anything unauthenticated.
//! 3. [`seq::ReplayWindow::accept`] — reject anything already seen.
//! 4. Decode the body and act on it.
//!
//! Checking the sequence before the MAC looks like a cheap denial-of-service
//! filter, but it lets unauthenticated traffic move the replay window, which is
//! the one thing it must not be able to do. The MAC costs about 100ns.

pub mod auth;
pub mod control;
pub mod header;
pub mod input;
pub mod rumble;
pub mod seq;

pub use auth::{MAC_LEN, NONCE_LEN, SessionKey};
pub use control::{ClientMessage, DecoderQuirks, ServerMessage};
pub use header::{Flags, HEADER_LEN, Header, MAX_PAYLOAD, PacketType};
pub use input::{GamepadState, InputEvent, InputKind, InputPacket, MouseButton, MouseMotion};
pub use rumble::{Rumble, RumbleTracker};
pub use seq::{REPLAY_WINDOW, ReplayWindow, Seq16};

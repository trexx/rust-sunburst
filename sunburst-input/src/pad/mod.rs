// SPDX-License-Identifier: GPL-2.0-or-later

//! The generic gamepad report codec — HIDMaestro's data-driven design, in Rust.
//!
//! A profile is JSON ([`spec`]); it compiles once to a field program
//! ([`program`]); the codec ([`codec`]) walks that program to pack a frame's
//! state into the profile's wire report, and to decode output reports back. One
//! codec serves every controller, so support is data, not code.
//!
//! Ported byte-for-byte from HIDMaestro (`VendorBlobProgram` + `VendorBlobCodec`,
//! MIT); the port lands op by op, verified against HIDMaestro's own golden report
//! hashes once the input direction is complete.

pub mod codec;
pub mod program;
pub mod spec;

pub use codec::{EncoderState, InputState, encode_input};
pub use program::{Program, compile};
pub use spec::ReportSpec;

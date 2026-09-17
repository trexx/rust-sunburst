// SPDX-License-Identifier: GPL-2.0-or-later

//! The generic gamepad report codec — HIDMaestro's data-driven design, in Rust.
//!
//! A profile is JSON ([`spec`]); it compiles once to a field program
//! ([`program`]); the codec ([`codec`]) walks that program to pack a frame's
//! state into the profile's wire report, and to decode output reports back. One
//! codec serves every controller, so support is data, not code.
//!
//! Ported byte-for-byte from HIDMaestro (`VendorBlobProgram` + `VendorBlobCodec`,
//! MIT) and proven so: [`golden`] reproduces all 63 of HIDMaestro's committed
//! report hashes across its nine Sony profiles, in every direction (input encode,
//! output encode, decode).

pub mod codec;
pub mod gip;
pub mod hid;
pub mod layout;
pub mod map;
pub mod outpolicy;
pub mod profile;
pub mod program;
pub mod registry;
pub mod report;
pub mod session;
pub mod shmem;
pub mod spec;
pub mod switch_pro;

#[cfg(test)]
mod descriptor_golden;
#[cfg(test)]
mod golden;

pub use codec::{
    DecodedValue, EncoderState, InputState, OutValue, decode, encode_input, encode_output,
};
pub use program::{Program, compile};
pub use spec::ReportSpec;

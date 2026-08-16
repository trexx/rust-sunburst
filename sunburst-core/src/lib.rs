// SPDX-License-Identifier: GPL-2.0-or-later

//! Protocol types, timestamps and instrumentation. No I/O.
//!
//! Shared by both ends of the link, which is why it holds the stage ids and the
//! clock as well as the wire format: the client reports timings against the same
//! [`instr::Stage`] values the server does.

pub mod instr;
pub mod proto;

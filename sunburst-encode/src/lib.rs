// SPDX-License-Identifier: GPL-2.0-or-later
#![cfg(windows)]

//! NVENC FFI, HEVC and AV1.
//!
//! Windows-only. On any other host this crate compiles to nothing, so
//! `cargo check` stays green on the Linux development machine while
//! `cargo xwin check --target x86_64-pc-windows-msvc` checks the real thing.
//! Neither command lies about the other.

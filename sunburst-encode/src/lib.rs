// SPDX-License-Identifier: GPL-2.0-or-later
#![cfg(windows)]

//! NVENC FFI, HEVC and AV1.
//!
//! Windows-only. On any other host this crate compiles to nothing, so
//! `cargo check` stays green on the Linux development machine while
//! `cargo xwin check --target x86_64-pc-windows-msvc` checks the real thing.
//! Neither command lies about the other.
//!
//! [`nvenc`] is the runtime-loaded NVENC binding (session open + caps today; the
//! encode loop — `initialize_encoder` / `register_resource` / `encode_picture` /
//! `lock_bitstream` — extends the function list from here). It opens a **DirectX**
//! session against the capture backend's `ID3D11Device` (`sunburst_capture`), so
//! each captured frame encodes without leaving the GPU.

pub mod av1c;
pub mod convert;
pub mod cuda_convert;
pub mod encoder;
pub mod hdr;
pub mod nvenc;

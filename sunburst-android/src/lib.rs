// SPDX-License-Identifier: GPL-2.0-or-later

//! Android client: network, depacketization, jitter buffer and decode driving.
//!
//! Kotlin owns only the Activity and its `SurfaceView` (plus the input and
//! platform-query callbacks that have no NDK equivalent); everything on the
//! frame path lives here. Built for `arm64-v8a` (Shield) and `armeabi-v7a`
//! (Homatics, which ships a 32-bit userspace on a 64-bit SoC).
//!
//! # Host vs. device
//!
//! The JNI shim and the `ndk`-crate decode/present are `#[cfg(target_os =
//! "android")]`, so on the Linux host this crate is an empty cdylib and its pure
//! logic (the input maps, the HDR byte assembly, the quirks derivation) still
//! compiles and unit-tests. The device work is exercised on the two TVs.

#[cfg(target_os = "android")]
mod client;
#[cfg(target_os = "android")]
mod decode;
#[cfg(target_os = "android")]
mod input;
#[cfg(target_os = "android")]
mod jni_bridge;
#[cfg(target_os = "android")]
mod pair;

// Pure, host-testable logic (no platform); public so the host build does not
// see it as dead when its only caller is Android-gated.
pub mod hdr_static_info;
pub mod input_map;
pub mod pin;

#[cfg(target_os = "android")]
pub use jni_bridge::*;

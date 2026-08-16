// SPDX-License-Identifier: GPL-2.0-or-later

//! Android client: network, depacketization, jitter buffer and decode driving.
//!
//! Kotlin owns only the Activity and its `SurfaceView`; everything on the frame
//! path lives here. Built for `arm64-v8a` (Shield) and `armeabi-v7a` (Homatics,
//! which ships a 32-bit userspace on a 64-bit SoC).

// SPDX-License-Identifier: GPL-2.0-or-later

//! UDP transport: packetization, pacing, NACK and rate control.
//!
//! Cross-platform, unlike the other server crates — the client needs the
//! receive and depacketize halves. Windows-only send offload (USO/URO) is
//! `cfg`-gated within, not at the crate boundary.

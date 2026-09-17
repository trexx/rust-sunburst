// SPDX-License-Identifier: GPL-2.0-or-later

//! Input injection: ViGEm, scancode `SendInput`, and desktop re-attach.
//!
//! # Not `#![cfg(windows)]` at the crate root
//!
//! Unlike the other server crates, only [`inject`] is Windows-only. The
//! decisions injection makes — which scancode, whether it is extended, how a
//! modifier bitfield reconciles, which `MOUSEEVENTF` bit an X button needs —
//! are pure functions in [`keymap`], and they are the parts that are actually
//! easy to get wrong.
//!
//! That means they carry real tests on the development machine even though
//! `SendInput` cannot be called here at all. `inject` is then a thin
//! transcription of what `keymap` decided, per CLAUDE.md's FFI rule.

pub mod keymap;
pub mod pad;

#[cfg(windows)]
pub mod inject;

#[cfg(windows)]
pub use inject::Injector;

// SPDX-License-Identifier: GPL-2.0-or-later

//! The bridge's error type. Deliberately small: opening a device is the only
//! fallible operation the caller cares about, and everything past it either
//! yields events or does not.

use std::fmt;

/// Why a [`Bridge`](crate::Bridge) could not be opened.
#[derive(Debug)]
pub enum Error {
    /// The USB device could not be claimed, firmware failed to load, or the
    /// radio would not come up. Carries the driver's reason.
    Open(String),
    /// This build has no real driver (the pure-Rust stub), so no device can be
    /// opened. Returned off-device so callers degrade instead of panicking.
    Unsupported,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Open(why) => write!(f, "gip-bridge: could not open device: {why}"),
            Error::Unsupported => write!(f, "gip-bridge: no driver in this build"),
        }
    }
}

impl std::error::Error for Error {}

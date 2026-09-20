// SPDX-License-Identifier: GPL-2.0-or-later

//! The pure-Rust stub driver: opens no device and produces no events.
//!
//! Compiled everywhere the real FFI driver is not (the Linux host, CI, and any
//! Android build without the `vendored` feature), so the crate and everything
//! that depends on it stay green with no C toolchain, libusb, or firmware. The
//! stub deliberately *succeeds* at opening — a client written against the
//! [`Bridge`](crate::Bridge) API can then be exercised end to end in host tests,
//! it simply never sees a pad.

use sunburst_core::proto::padoutput::PadOutput;
use sunburst_core::proto::rumble::Rumble;

use crate::{AudioFormat, Error, MicFormat, PadEvent, RawFd};

pub struct Driver;

impl Driver {
    pub fn open_dongle(_fd: RawFd, _firmware_path: &str) -> Result<Driver, Error> {
        Ok(Driver)
    }

    pub fn open_wired(_fd: RawFd) -> Result<Driver, Error> {
        Ok(Driver)
    }

    pub fn poll(&self) -> Option<PadEvent> {
        None
    }

    pub fn rumble(&self, _r: Rumble) {}

    pub fn pad_output(&self, _o: PadOutput) {}

    pub fn set_pairing(&self, _on: bool) -> bool {
        false
    }

    pub fn audio_format(&self, _index: u8) -> Option<AudioFormat> {
        None
    }

    pub fn mic_format(&self, _index: u8) -> Option<MicFormat> {
        None
    }

    pub fn set_audio_enabled(&self, _index: u8, _on: bool) -> bool {
        false
    }

    pub fn set_audio_volume(&self, _index: u8, _percent: u8) -> bool {
        false
    }

    pub fn audio_out(&self, _index: u8, _samples: &[i16]) {}

    pub fn audio_in(&self, _index: u8, _out: &mut [i16]) -> usize {
        0
    }
}

// SPDX-License-Identifier: GPL-2.0-or-later

//! The C FFI seam to the vendored xow-derived driver.
//!
//! Compiled only under `--features vendored` on `target_os = "android"`. The C++
//! side (`vendor/shim/sb_gip_shim.cpp`, over the xow `dongle`/`controller`/
//! `wired`/`crypto` sources and libusb) exports the plain-C functions below —
//! this replaces xow's Android JNI layer with a Rust-facing seam, so Kotlin owns
//! only the USB permission + fd (Sunburst's "Kotlin owns only the Activity").
//! `build.rs` compiles it with `cc`.
//!
//! The whole `unsafe` surface of the feature lives here; everything above is safe.

use std::ffi::{CString, c_char, c_int, c_void};

use sunburst_core::proto::input::{Battery, GamepadState, battery_flags};
use sunburst_core::proto::padoutput::PadOutput;
use sunburst_core::proto::rumble::Rumble;

use crate::{AudioFormat, Error, MicFormat, PadEvent, RawFd};

/// Mirror of the C `SbPadEvent`. `kind`: 0 none, 1 connected, 2 disconnected,
/// 3 input. Fields past `index` are valid only for `kind == 3`.
#[repr(C)]
#[derive(Clone, Copy)]
struct SbPadEvent {
    kind: u8,
    index: u8,
    buttons: u32,
    lx: i16,
    ly: i16,
    rx: i16,
    ry: i16,
    lt: u8,
    rt: u8,
    battery_present: u8,
    battery_level: u8,
    battery_flags: u8,
}

impl SbPadEvent {
    const EMPTY: SbPadEvent = SbPadEvent {
        kind: 0,
        index: 0,
        buttons: 0,
        lx: 0,
        ly: 0,
        rx: 0,
        ry: 0,
        lt: 0,
        rt: 0,
        battery_present: 0,
        battery_level: 0,
        battery_flags: 0,
    };
}

// SAFETY: the C shim guarantees these signatures; a mismatch is a link error, by
// design (the C header and this block are the two halves of the same contract).
unsafe extern "C" {
    fn sb_gip_open_dongle(fd: c_int, firmware_path: *const c_char) -> *mut c_void;
    fn sb_gip_open_wired(fd: c_int) -> *mut c_void;
    fn sb_gip_poll(handle: *mut c_void, out: *mut SbPadEvent) -> c_int;
    fn sb_gip_rumble(handle: *mut c_void, pad: u8, low: u16, high: u16, trig_l: u16, trig_r: u16);
    fn sb_gip_set_pairing(handle: *mut c_void, on: bool) -> bool;
    /// -1 = no headset, 0 = 48 kHz mono, 1 = 48 kHz stereo.
    fn sb_gip_audio_format(handle: *mut c_void, pad: u8) -> c_int;
    /// Raw capture (mic) format code (MS-GIPUSB 3.2.5.1.2); 0 = no mic, -1 = no pad.
    fn sb_gip_mic_format(handle: *mut c_void, pad: u8) -> c_int;
    fn sb_gip_audio_set_enabled(handle: *mut c_void, pad: u8, on: bool) -> bool;
    fn sb_gip_audio_set_volume(handle: *mut c_void, pad: u8, percent: u8) -> bool;
    fn sb_gip_audio_out(handle: *mut c_void, pad: u8, samples: *const i16, n: usize);
    fn sb_gip_audio_in(handle: *mut c_void, pad: u8, out: *mut i16, cap: usize) -> usize;
    fn sb_gip_close(handle: *mut c_void);
}

pub struct Driver {
    handle: *mut c_void,
}

// SAFETY: the C++ shim serialises every call on the handle through its own mutex
// (see vendor/shim/sb_gip_shim.cpp), so the handle is safe to move between threads.
unsafe impl Send for Driver {}
// SAFETY: same — every call goes through the shim's mutex, so sharing the handle
// by `&` across threads is sound. That is why the methods below take `&self`.
unsafe impl Sync for Driver {}

impl Driver {
    pub fn open_dongle(fd: RawFd, firmware_path: &str) -> Result<Driver, Error> {
        let path = CString::new(firmware_path)
            .map_err(|_| Error::Open("firmware path has an interior NUL".into()))?;
        // SAFETY: `fd` is a live UsbManager fd; `path` outlives the call.
        let handle = unsafe { sb_gip_open_dongle(fd, path.as_ptr()) };
        Self::from_handle(handle)
    }

    pub fn open_wired(fd: RawFd) -> Result<Driver, Error> {
        // SAFETY: `fd` is a live UsbManager fd.
        let handle = unsafe { sb_gip_open_wired(fd) };
        Self::from_handle(handle)
    }

    fn from_handle(handle: *mut c_void) -> Result<Driver, Error> {
        if handle.is_null() {
            Err(Error::Open("driver returned null (see logcat)".into()))
        } else {
            Ok(Driver { handle })
        }
    }

    pub fn poll(&self) -> Option<PadEvent> {
        let mut ev = SbPadEvent::EMPTY;
        // SAFETY: `handle` is live; `ev` is a valid out-param for one event.
        let wrote = unsafe { sb_gip_poll(self.handle, &mut ev) };
        if wrote == 0 {
            return None;
        }
        match ev.kind {
            1 => Some(PadEvent::Connected { index: ev.index }),
            2 => Some(PadEvent::Disconnected { index: ev.index }),
            3 => Some(PadEvent::Input(gamepad_from(&ev))),
            _ => None,
        }
    }

    pub fn rumble(&self, r: Rumble) {
        // SAFETY: `handle` is live; the call copies the scalars.
        unsafe {
            sb_gip_rumble(
                self.handle,
                r.pad_index,
                r.motor_low,
                r.motor_high,
                r.trigger_left,
                r.trigger_right,
            );
        }
    }

    pub fn pad_output(&self, o: PadOutput) {
        // Xbox pads have no adaptive triggers/LED; forward the motor pair (incl.
        // nothing extra) so a rich frame still drives rumble on the pad.
        self.rumble(Rumble {
            pad_index: o.pad_index,
            motor_low: o.motor_low,
            motor_high: o.motor_high,
            ..Default::default()
        });
    }

    pub fn set_pairing(&self, on: bool) -> bool {
        // SAFETY: `handle` is live.
        unsafe { sb_gip_set_pairing(self.handle, on) }
    }

    pub fn audio_format(&self, index: u8) -> Option<AudioFormat> {
        // SAFETY: `handle` is live.
        match unsafe { sb_gip_audio_format(self.handle, index) } {
            0 => Some(AudioFormat::Mono48k),
            1 => Some(AudioFormat::Stereo48k),
            _ => None,
        }
    }

    pub fn mic_format(&self, index: u8) -> Option<MicFormat> {
        // SAFETY: `handle` is live. The shim returns the raw capture format code;
        // the spec table lives in `decode_audio_format` so it is host-tested.
        let code = unsafe { sb_gip_mic_format(self.handle, index) };
        crate::decode_audio_format(code)
    }

    pub fn set_audio_enabled(&self, index: u8, on: bool) -> bool {
        // SAFETY: `handle` is live.
        unsafe { sb_gip_audio_set_enabled(self.handle, index, on) }
    }

    pub fn set_audio_volume(&self, index: u8, percent: u8) -> bool {
        // SAFETY: `handle` is live.
        unsafe { sb_gip_audio_set_volume(self.handle, index, percent) }
    }

    pub fn audio_out(&self, index: u8, samples: &[i16]) {
        // SAFETY: `handle` is live; the shim copies `samples[..n]` this call.
        unsafe { sb_gip_audio_out(self.handle, index, samples.as_ptr(), samples.len()) }
    }

    pub fn audio_in(&self, index: u8, out: &mut [i16]) -> usize {
        // SAFETY: `handle` is live; the shim writes at most `out.len()` samples.
        unsafe { sb_gip_audio_in(self.handle, index, out.as_mut_ptr(), out.len()) }
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        // SAFETY: `handle` came from an open call and is closed exactly once.
        unsafe { sb_gip_close(self.handle) }
    }
}

/// Build a [`GamepadState`] from a C input event.
fn gamepad_from(ev: &SbPadEvent) -> GamepadState {
    let battery = (ev.battery_present != 0).then_some(Battery {
        level: ev.battery_level,
        charging: ev.battery_flags & battery_flags::CHARGING != 0,
        full: ev.battery_flags & battery_flags::FULL != 0,
        mic_muted: ev.battery_flags & battery_flags::MIC_MUTED != 0,
        headphones: ev.battery_flags & battery_flags::HEADPHONES != 0,
    });
    GamepadState {
        pad_index: ev.index,
        buttons: ev.buttons,
        lx: ev.lx,
        ly: ev.ly,
        rx: ev.rx,
        ry: ev.ry,
        lt: ev.lt,
        rt: ev.rt,
        imu: None,
        touchpad: None,
        battery,
    }
}

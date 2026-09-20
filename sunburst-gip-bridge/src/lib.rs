// SPDX-License-Identifier: GPL-2.0-or-later

//! Xbox controllers over the wireless adapter and wired USB, for the Android
//! client.
//!
//! This crate is the **narrow seam** around a vendored, hardware-proven driver:
//! the MT7612U radio bring-up, the GIP protocol, the wired path, the security
//! handshake and headset audio all live in C++ (xow lineage, GPL-2-or-later; see
//! `vendor/UPSTREAM.md` once vendored), reached through a small C FFI. Rust owns
//! the safe API and maps the driver's decoded pads into Sunburst's protocol
//! types ([`sunburst_core::proto`]). Keeping all the `unsafe`/C behind one crate
//! is CLAUDE.md's "wrap unsafe thin and early, at the crate boundary".
//!
//! # Host vs. device
//!
//! The real driver compiles only for `target_os = "android"` **and** the
//! `vendored` feature (the only place it runs, and the only build with the C
//! toolchain + firmware). Everywhere else — the Linux dev host, CI, `cargo test`
//! — [`Bridge`] is a pure-Rust **stub** that opens no device and yields no
//! events, so the whole workspace stays green without libusb or the NDK, exactly
//! as `sunburst-android` compiles to an empty cdylib off-device.
//!
//! # Status
//!
//! The seam and the stub are in place; the C++ vendor-import + `cc` build script
//! that turn the `vendored` feature real are Stage B1's second half. Until then
//! every target builds against the stub.

use sunburst_core::proto::input::GamepadState;
use sunburst_core::proto::padoutput::PadOutput;
use sunburst_core::proto::rumble::Rumble;

mod error;
pub use error::Error;

pub mod crypto;

/// A raw file descriptor — Android's `UsbManager` hands one down for the claimed
/// device. Aliased to a C `int` locally rather than through `std::os::fd` so the
/// crate still *compiles* on non-Unix hosts (the workspace's Windows cross-check
/// builds every member); it only ever *runs* on Android.
pub type RawFd = std::ffi::c_int;

// The real FFI driver, or the stub — never both. The public [`Bridge`] wrapper
// is identical over either, so the client code never sees which is compiled.
#[cfg(all(target_os = "android", feature = "vendored"))]
#[path = "ffi.rs"]
mod imp;
#[cfg(not(all(target_os = "android", feature = "vendored")))]
#[path = "stub.rs"]
mod imp;

/// The headset audio format a pad negotiated (GIP `SupportedAudioFormats`,
/// `[MS-GIPUSB]` 2.2.2.4.3). Both are 48 kHz, so no resampling against the
/// stream — a mono headset only needs a stereo→mono downmix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioFormat {
    /// 48 kHz mono (`0x0f`).
    Mono48k,
    /// 48 kHz stereo (`0x10`).
    Stereo48k,
}

/// The microphone (capture) audio format a pad's headset negotiated. Unlike
/// [`AudioFormat`] (the render/speaker side, always 48 kHz here), the mic runs at
/// its own rate and channel count — a chat headset is typically 24 kHz mono
/// (`[MS-GIPUSB]` 3.2.5.1.2, format code `0x09`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MicFormat {
    /// Sample rate in Hz (8000/12000/16000/20000/24000/32000/40000/48000).
    pub rate: u32,
    /// 1 (mono) or 2 (stereo).
    pub channels: u8,
}

/// Decode a `[MS-GIPUSB]` 3.2.5.1.2 audio format code into a rate and channel
/// count, or `None` for "no audio" (`0`) or an unknown/out-of-range code.
///
/// The codes run `0x01..=0x10` as `(rate, channels)` pairs in ascending rate:
/// odd codes are 1 channel, even are 2, and the rate index is `(code + 1) / 2`
/// over `[8, 12, 16, 20, 24, 32, 40, 48] kHz`. So `0x09` → 24 kHz mono, `0x10` →
/// 48 kHz stereo, `0x0f` → 48 kHz mono. Kept in Rust (not the C++ shim) so the
/// table is host-tested; the shim passes the raw code through `sb_gip_mic_format`.
///
/// `pub` because it is the public counterpart of [`MicFormat`], and because its only
/// non-test caller ([`ffi`]) is compiled only on Android — keeping it `pub(crate)`
/// would read as dead code on the host/stub build.
pub fn decode_audio_format(code: i32) -> Option<MicFormat> {
    if !(1..=0x10).contains(&code) {
        return None;
    }
    const RATES: [u32; 9] = [0, 8000, 12000, 16000, 20000, 24000, 32000, 40000, 48000];
    let idx = ((code + 1) / 2) as usize;
    let rate = RATES[idx];
    let channels = 2 - (code & 1) as u8;
    Some(MicFormat { rate, channels })
}

/// One update drained from the driver by [`Bridge::poll`].
#[derive(Clone, Debug, PartialEq)]
pub enum PadEvent {
    /// A pad arrived on slot `index` (0..[`MAX_PADS`](sunburst_core::proto::input::MAX_PADS)).
    Connected { index: u8 },
    /// The pad on slot `index` went away.
    Disconnected { index: u8 },
    /// A fresh input report; `state.pad_index` names the slot.
    Input(GamepadState),
}

/// A running driver instance owning one USB device — a wireless adapter serving
/// up to four pads, or a single wired pad. Closes the device on drop.
pub struct Bridge {
    driver: imp::Driver,
}

impl Bridge {
    /// Open the Xbox **Wireless Adapter** on a UsbManager file descriptor,
    /// loading MT7612U firmware from `firmware_path` (fetched, never committed).
    /// The fd is owned by the driver until the [`Bridge`] is dropped.
    pub fn open_dongle(fd: RawFd, firmware_path: &str) -> Result<Bridge, Error> {
        Ok(Bridge {
            driver: imp::Driver::open_dongle(fd, firmware_path)?,
        })
    }

    /// Open a **wired** Xbox One/Series pad on a UsbManager file descriptor.
    /// No radio and no firmware — the pad speaks GIP directly over USB.
    pub fn open_wired(fd: RawFd) -> Result<Bridge, Error> {
        Ok(Bridge {
            driver: imp::Driver::open_wired(fd)?,
        })
    }

    /// Drain the next pending event, or `None` if there is nothing right now.
    /// Non-blocking: the caller pumps this from the client loop.
    ///
    /// Takes `&self`: the C++ shim serialises every call through its own mutex, so
    /// a single `Arc<Bridge>` can be shared across the UI thread (pairing), the
    /// client loop (poll) and the pad sink (rumble) without a Rust-side lock.
    pub fn poll(&self) -> Option<PadEvent> {
        self.driver.poll()
    }

    /// Set a pad's motor levels, including the Xbox impulse-trigger motors
    /// carried in [`Rumble`].
    pub fn rumble(&self, r: Rumble) {
        self.driver.rumble(r);
    }

    /// Apply a rich output frame (motors + adaptive triggers + LED). Xbox pads
    /// use [`Self::rumble`]; this is here for completeness of the seam.
    pub fn pad_output(&self, o: PadOutput) {
        self.driver.pad_output(o);
    }

    /// Enter or leave pairing mode (wireless adapter only). Returns whether the
    /// request was accepted.
    pub fn set_pairing(&self, on: bool) -> bool {
        self.driver.set_pairing(on)
    }

    /// The negotiated headset format for a pad, or `None` if it has no headset
    /// (or none yet — the audio sub-device appears only after the handshake).
    /// Used as "does this pad have a headset"; the driver takes stereo regardless.
    pub fn audio_format(&self, index: u8) -> Option<AudioFormat> {
        self.driver.audio_format(index)
    }

    /// The pad's headset **microphone** (capture) format, or `None` if it has no
    /// mic. Distinct from [`Self::audio_format`] (the speaker/render side): the mic
    /// is often 24 kHz mono where the speaker is 48 kHz stereo, so the client
    /// encodes the mic at *this* rate/channels and the server's 48 kHz Opus decoder
    /// resamples it.
    pub fn mic_format(&self, index: u8) -> Option<MicFormat> {
        self.driver.mic_format(index)
    }

    /// Enable or disable a pad's audio sub-device. Must be enabled (which
    /// negotiates the format and starts the sender) before [`Self::audio_out`]
    /// queues anything. Returns whether the request took.
    pub fn set_audio_enabled(&self, index: u8, on: bool) -> bool {
        self.driver.set_audio_enabled(index, on)
    }

    /// Set a pad's headset speaker volume, `0..=100`. Returns whether the pad
    /// accepted it (it attenuates in software otherwise).
    pub fn set_audio_volume(&self, index: u8, percent: u8) -> bool {
        self.driver.set_audio_volume(index, percent)
    }

    /// Queue headset playback samples — **interleaved 48 kHz stereo i16**; the
    /// driver handles the pad's negotiated mono/stereo itself.
    pub fn audio_out(&self, index: u8, samples: &[i16]) {
        self.driver.audio_out(index, samples);
    }

    /// Read available headset-mic samples for pad `index` into `out`, returning
    /// the number of samples written (0 if none are ready).
    pub fn audio_in(&self, index: u8, out: &mut [i16]) -> usize {
        self.driver.audio_in(index, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The stub is what host CI exercises: the API is usable and inert, so a
    // client can be written and tested against it without a device.
    #[test]
    fn the_stub_opens_and_is_inert() {
        let b = Bridge::open_wired(-1).expect("stub opens");
        assert_eq!(b.poll(), None);
        assert!(!b.set_pairing(true));
        assert_eq!(b.audio_format(0), None);
        assert!(!b.set_audio_enabled(0, true));
        assert!(!b.set_audio_volume(0, 80));
        b.rumble(Rumble::default());
        b.audio_out(0, &[0i16; 240]);
        assert_eq!(b.audio_in(0, &mut [0i16; 240]), 0);
        assert_eq!(b.mic_format(0), None);
    }

    #[test]
    fn audio_format_codes_decode_per_the_spec_table() {
        // MS-GIPUSB 3.2.5.1.2: the codes measured on a real headset and the extremes.
        assert_eq!(
            decode_audio_format(0x09),
            Some(MicFormat {
                rate: 24000,
                channels: 1
            }),
            "0x09 is 24 kHz mono — the Xbox chat mic"
        );
        assert_eq!(
            decode_audio_format(0x10),
            Some(MicFormat {
                rate: 48000,
                channels: 2
            }),
            "0x10 is 48 kHz stereo — the headset speaker"
        );
        assert_eq!(
            decode_audio_format(0x0f),
            Some(MicFormat {
                rate: 48000,
                channels: 1
            })
        );
        assert_eq!(
            decode_audio_format(0x01),
            Some(MicFormat {
                rate: 8000,
                channels: 1
            })
        );
        assert_eq!(
            decode_audio_format(0x0a),
            Some(MicFormat {
                rate: 24000,
                channels: 2
            })
        );
        // 0 (no audio), negative (no such pad), and out-of-range all decode to None.
        assert_eq!(decode_audio_format(0), None);
        assert_eq!(decode_audio_format(-1), None);
        assert_eq!(decode_audio_format(0x11), None);
    }
}

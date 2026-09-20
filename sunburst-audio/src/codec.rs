// SPDX-License-Identifier: GPL-2.0-or-later

//! Thin safe wrappers over the transpiled libopus, one direction each.
//!
//! Per CLAUDE.md's FFI rule the unsafe is kept thin and at the boundary: the
//! wrapper owns the raw `*mut OpusEncoder`/`*mut OpusDecoder`, frees it on drop,
//! and every call is a checked, slice-based method. The codec itself is pure
//! Rust (`unsafe-libopus`), so this module is cross-platform and host-tested —
//! the server links [`OpusEncoder`], the Android client links [`OpusDecoder`].
//!
//! Both wrappers are `Send` but not `Sync`: each encoder/decoder is owned by one
//! thread (the audio pipeline thread on the server, the client thread on
//! Android), which is how libopus is meant to be used.

use core::ffi::c_int;

// The crate name carries "unsafe" as a warning about its transpiled contents;
// the alias is only for brevity at the call sites, which stay `unsafe`.
#[allow(clippy::unsafe_removed_from_name)]
use unsafe_libopus as ffi;
// The encoder ctl entry point is a variadic C macro; the crate exposes it as a
// Rust `macro_rules!` at its root.
use unsafe_libopus::opus_encoder_ctl;

/// An Opus library error, carrying the raw negative status code.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OpusError(pub i32);

impl core::fmt::Display for OpusError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The transpiled `opus_strerror` returns a static &str, not a C pointer.
        write!(f, "opus error {}: {}", self.0, ffi::opus_strerror(self.0))
    }
}

impl std::error::Error for OpusError {}

/// The Opus coding mode. Game streaming wants [`Application::LowDelay`], which
/// disables the SILK look-ahead (`OPUS_APPLICATION_RESTRICTED_LOWDELAY`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Application {
    LowDelay,
    Audio,
    Voip,
}

impl Application {
    const fn raw(self) -> c_int {
        match self {
            Application::LowDelay => ffi::OPUS_APPLICATION_RESTRICTED_LOWDELAY,
            Application::Audio => ffi::OPUS_APPLICATION_AUDIO,
            Application::Voip => ffi::OPUS_APPLICATION_VOIP,
        }
    }
}

/// An Opus encoder for one stream.
pub struct OpusEncoder {
    raw: *mut ffi::OpusEncoder,
    channels: usize,
}

// SAFETY: the encoder state is owned exclusively by this wrapper and is not
// shared; libopus permits moving an encoder between threads as long as it is
// used from one at a time, which `&mut self` on every method guarantees.
unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    /// Create an encoder at `sample_rate` Hz with `channels` channels.
    pub fn new(
        sample_rate: u32,
        channels: u8,
        application: Application,
    ) -> Result<OpusEncoder, OpusError> {
        let mut err: c_int = 0;
        // SAFETY: `err` is a valid out-pointer; the returned pointer is checked
        // against the status before use.
        let raw = unsafe {
            ffi::opus_encoder_create(
                sample_rate as c_int,
                channels as c_int,
                application.raw(),
                &mut err,
            )
        };
        if err != ffi::OPUS_OK || raw.is_null() {
            return Err(OpusError(err));
        }
        Ok(OpusEncoder {
            raw,
            channels: channels as usize,
        })
    }

    fn ctl(&mut self, request: c_int, value: c_int) -> Result<(), OpusError> {
        // SAFETY: `self.raw` is a live encoder; the ctl macro forwards `value`
        // by copy to the requested setter.
        let ret = unsafe { opus_encoder_ctl!(self.raw, request, value) };
        (ret == ffi::OPUS_OK).then_some(()).ok_or(OpusError(ret))
    }

    /// Set the target bitrate in bits per second.
    pub fn set_bitrate(&mut self, bits_per_sec: u32) -> Result<(), OpusError> {
        self.ctl(ffi::OPUS_SET_BITRATE_REQUEST, bits_per_sec as c_int)
    }

    /// Enable Opus in-band forward error correction, so a lost packet can be
    /// recovered from the low-bitrate copy in the next one.
    pub fn set_inband_fec(&mut self, on: bool) -> Result<(), OpusError> {
        self.ctl(ffi::OPUS_SET_INBAND_FEC_REQUEST, c_int::from(on))
    }

    /// Tell the encoder the expected packet-loss percentage, which is what makes
    /// FEC actually spend bits on recovery data.
    pub fn set_packet_loss_perc(&mut self, pct: u8) -> Result<(), OpusError> {
        self.ctl(
            ffi::OPUS_SET_PACKET_LOSS_PERC_REQUEST,
            pct.min(100) as c_int,
        )
    }

    /// Set the computational complexity (0..=10).
    pub fn set_complexity(&mut self, complexity: u8) -> Result<(), OpusError> {
        self.ctl(
            ffi::OPUS_SET_COMPLEXITY_REQUEST,
            complexity.min(10) as c_int,
        )
    }

    /// Encode one frame of interleaved i16 PCM into `out`, returning the packet
    /// length in bytes. `pcm.len()` must be `frame_samples * channels`.
    pub fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize, OpusError> {
        let frame_size = pcm.len() / self.channels;
        debug_assert_eq!(
            frame_size * self.channels,
            pcm.len(),
            "PCM length must be a whole number of frames"
        );
        // SAFETY: `pcm`/`out` are valid slices; `frame_size` is derived from
        // `pcm.len()`, and `out.len()` bounds the writable region.
        let n = unsafe {
            ffi::opus_encode(
                self.raw,
                pcm.as_ptr(),
                frame_size as c_int,
                out.as_mut_ptr(),
                out.len() as c_int,
            )
        };
        if n < 0 {
            Err(OpusError(n))
        } else {
            Ok(n as usize)
        }
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        // SAFETY: `self.raw` came from `opus_encoder_create` and is freed once.
        unsafe { ffi::opus_encoder_destroy(self.raw) };
    }
}

/// An Opus decoder for one stream.
pub struct OpusDecoder {
    raw: *mut ffi::OpusDecoder,
    channels: usize,
}

// SAFETY: as with the encoder — single-owner state, `&mut self` on every call.
unsafe impl Send for OpusDecoder {}

impl OpusDecoder {
    /// Create a decoder at `sample_rate` Hz with `channels` channels.
    pub fn new(sample_rate: u32, channels: u8) -> Result<OpusDecoder, OpusError> {
        let mut err: c_int = 0;
        // SAFETY: `err` is a valid out-pointer; the result is checked.
        let raw =
            unsafe { ffi::opus_decoder_create(sample_rate as c_int, channels as c_int, &mut err) };
        if err != ffi::OPUS_OK || raw.is_null() {
            return Err(OpusError(err));
        }
        Ok(OpusDecoder {
            raw,
            channels: channels as usize,
        })
    }

    /// Decode one packet into `out` (interleaved i16), returning samples per
    /// channel. `out` must hold at least one frame: `frame_samples * channels`.
    ///
    /// With `fec = true` the decoder instead extracts the *previous* frame from
    /// this packet's forward-error-correction data, to recover a single loss.
    pub fn decode(
        &mut self,
        packet: &[u8],
        out: &mut [i16],
        fec: bool,
    ) -> Result<usize, OpusError> {
        let frame_size = out.len() / self.channels;
        // SAFETY: `packet`/`out` are valid slices with lengths passed through;
        // `frame_size` bounds the write.
        let n = unsafe {
            ffi::opus_decode(
                self.raw,
                packet.as_ptr(),
                packet.len() as c_int,
                out.as_mut_ptr(),
                frame_size as c_int,
                c_int::from(fec),
            )
        };
        if n < 0 {
            Err(OpusError(n))
        } else {
            Ok(n as usize)
        }
    }

    /// Conceal one lost packet of `frame_samples` per channel (packet-loss
    /// concealment), writing interpolated PCM into `out`.
    pub fn conceal(&mut self, out: &mut [i16], frame_samples: usize) -> Result<usize, OpusError> {
        debug_assert!(out.len() >= frame_samples * self.channels);
        // SAFETY: a null data pointer with length 0 is libopus's documented PLC
        // request; `out` holds `frame_samples * channels`.
        let n = unsafe {
            ffi::opus_decode(
                self.raw,
                core::ptr::null(),
                0,
                out.as_mut_ptr(),
                frame_samples as c_int,
                0,
            )
        };
        if n < 0 {
            Err(OpusError(n))
        } else {
            Ok(n as usize)
        }
    }

    /// Reset the packet-loss expectation the encoder side set is not needed
    /// here; the decoder is stateless across sessions once dropped.
    pub fn channels(&self) -> usize {
        self.channels
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        // SAFETY: `self.raw` came from `opus_decoder_create` and is freed once.
        unsafe { ffi::opus_decoder_destroy(self.raw) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CH: usize = 2;
    const FRAME: usize = 240; // 5 ms at 48 kHz

    fn tone(n: usize) -> Vec<i16> {
        (0..n * CH)
            .map(|i| ((i as f32 * 0.05).sin() * 8000.0) as i16)
            .collect()
    }

    #[test]
    fn encode_then_decode_recovers_a_frame() {
        let mut enc = OpusEncoder::new(48_000, 2, Application::LowDelay).expect("encoder");
        enc.set_bitrate(128_000).unwrap();
        enc.set_inband_fec(true).unwrap();
        enc.set_packet_loss_perc(5).unwrap();

        let mut dec = OpusDecoder::new(48_000, 2).expect("decoder");

        let pcm = tone(FRAME);
        let mut packet = [0u8; 1275];
        let bytes = enc.encode(&pcm, &mut packet).expect("encode");
        assert!(bytes > 1 && bytes < 1000, "plausible packet: {bytes}");

        let mut out = [0i16; FRAME * CH];
        let samples = dec
            .decode(&packet[..bytes], &mut out, false)
            .expect("decode");
        assert_eq!(samples, FRAME);
    }

    #[test]
    fn silence_encodes_and_round_trips() {
        let mut enc = OpusEncoder::new(48_000, 2, Application::LowDelay).unwrap();
        let mut dec = OpusDecoder::new(48_000, 2).unwrap();
        let silence = [0i16; FRAME * CH];
        let mut packet = [0u8; 1275];
        let bytes = enc.encode(&silence, &mut packet).unwrap();
        let mut out = [0i16; FRAME * CH];
        assert_eq!(
            dec.decode(&packet[..bytes], &mut out, false).unwrap(),
            FRAME
        );
    }

    #[test]
    fn concealment_produces_a_frame() {
        let mut dec = OpusDecoder::new(48_000, 2).unwrap();
        let mut out = [0i16; FRAME * CH];
        assert_eq!(dec.conceal(&mut out, FRAME).unwrap(), FRAME);
    }

    #[test]
    fn a_bad_sample_rate_is_refused() {
        assert!(OpusEncoder::new(44_100, 2, Application::LowDelay).is_err());
    }
}

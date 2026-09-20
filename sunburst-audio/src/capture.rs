// SPDX-License-Identifier: GPL-2.0-or-later

//! WASAPI loopback capture of a render endpoint.
//!
//! Shared-mode `IAudioClient3` initialised with `AUDCLNT_STREAMFLAGS_LOOPBACK`
//! captures the mix being played to the endpoint. Loopback has no event
//! callback and delivers **nothing** while the endpoint is idle
//! (`GetNextPacketSize` returns 0) — it does not emit silence — so the caller
//! polls on a short tick and injects silence itself to hold the A/V cadence (see
//! the server's audio pipeline).
//!
//! The shared mix format is the endpoint's, not ours: it is essentially always
//! 48 kHz float32, which is why CLAUDE.md asks for a 48 kHz endpoint. This code
//! converts float32 or 16-bit PCM down to stereo i16 (via [`crate::pcm`]) and
//! warns on anything else. Timestamps are **not** taken here: the pipeline
//! stamps each outgoing frame with the raw performance counter so audio shares
//! video's clock domain (WASAPI's own QPC position is normalised to 100 ns, a
//! different domain).

use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
    IAudioCaptureClient, IAudioClient3, WAVE_FORMAT_PCM, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::core::{GUID, Result};

use crate::device::select_render_endpoint;
use crate::pcm;

/// `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT`, defined here rather than pulling the whole
/// `Win32_Media_Multimedia` module for one fixed GUID.
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

/// The tag value for `WAVE_FORMAT_IEEE_FLOAT` (from `Win32_Media_Multimedia`).
const WAVE_FORMAT_IEEE_FLOAT_TAG: u16 = 3;

/// The endpoint's captured sample format, parsed from the shared mix format.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CaptureFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub is_float: bool,
    pub bits: u16,
}

/// A live WASAPI loopback capture.
pub struct LoopbackCapture {
    // Kept alive: the capture client is a service of this audio client.
    _client: IAudioClient3,
    capture: IAudioCaptureClient,
    format: CaptureFormat,
    /// Scratch for the converted stereo i16 of one WASAPI packet, so draining
    /// does not allocate after warmup.
    scratch: Vec<i16>,
    /// Whether this instance called `CoInitializeEx` and must balance it.
    owns_com: bool,
}

impl LoopbackCapture {
    /// Open loopback capture on the selected render endpoint (see
    /// [`select_render_endpoint`]). Call from the thread that will drain it —
    /// COM is initialised on the calling thread.
    pub fn open(device_name: Option<&str>) -> Result<LoopbackCapture> {
        // SAFETY: initialise COM (multithreaded) for this thread. S_OK means we
        // own the initialisation and must uninit on drop; S_FALSE / an existing
        // apartment means we do not.
        let owns_com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).is_ok() };

        let device = select_render_endpoint(device_name)?;

        // SAFETY: activate the audio client, read and apply the shared mix
        // format with the loopback flag, take the capture service, and start.
        // The mix format is allocated by GetMixFormat and freed after Initialize
        // has copied it.
        unsafe {
            let client: IAudioClient3 = device.Activate(CLSCTX_ALL, None)?;
            let mix = client.GetMixFormat()?;
            let format = parse_format(&*mix);

            if format.sample_rate != 48_000 {
                log::warn!(
                    "audio: endpoint is {} Hz, not 48000; set the server output to 48 kHz \
                     (resampling is not implemented)",
                    format.sample_rate
                );
            }
            if !format.is_float && format.bits != 16 {
                log::warn!(
                    "audio: endpoint format is {}-bit integer; only float32 and 16-bit are \
                     converted, other frames will be treated as silence",
                    format.bits
                );
            }

            // 200 ms shared buffer; loopback ignores periodicity (0).
            let hns_buffer = 200 * 10_000;
            let init = client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                hns_buffer,
                0,
                mix,
                None,
            );
            CoTaskMemFree(Some(mix as *const _));
            init?;

            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;

            Ok(LoopbackCapture {
                _client: client,
                capture,
                format,
                scratch: Vec::with_capacity(4096),
                owns_com,
            })
        }
    }

    /// The captured endpoint format.
    pub fn format(&self) -> CaptureFormat {
        self.format
    }

    /// Drain every WASAPI packet currently available, pushing the converted
    /// stereo i16 into `acc`. Returns the number of source frames drained (0 when
    /// the endpoint is idle — the caller injects silence then).
    pub fn drain_into(&mut self, acc: &mut pcm::FrameAccumulator) -> Result<u32> {
        let mut drained = 0u32;
        loop {
            // SAFETY: GetNextPacketSize reports the next packet's frame count;
            // zero means nothing is buffered.
            let next = unsafe { self.capture.GetNextPacketSize()? };
            if next == 0 {
                break;
            }

            let mut data: *mut u8 = core::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;
            // SAFETY: GetBuffer hands back a read-only view of `frames` frames at
            // `data`, valid until ReleaseBuffer; device/QPC positions are unused.
            unsafe {
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
            }

            let channels = self.format.channels as usize;
            self.scratch.clear();
            if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data.is_null() {
                // Silent packet: the buffer contents are undefined, so synthesise
                // zeroed stereo rather than reading it.
                self.scratch.resize(frames as usize * 2, 0);
            } else if self.format.is_float {
                // SAFETY: a silent-flag-free float32 packet is `frames*channels`
                // f32 samples at `data`.
                let src = unsafe {
                    core::slice::from_raw_parts(data as *const f32, frames as usize * channels)
                };
                pcm::convert_to_stereo_i16(src, channels, &mut self.scratch);
            } else if self.format.bits == 16 {
                // SAFETY: 16-bit PCM packet is `frames*channels` i16 at `data`.
                let src = unsafe {
                    core::slice::from_raw_parts(data as *const i16, frames as usize * channels)
                };
                pcm::downmix_i16_to_stereo(src, channels, &mut self.scratch);
            } else {
                // Unsupported bit depth: treat as silence to hold cadence.
                self.scratch.resize(frames as usize * 2, 0);
            }
            acc.push(&self.scratch);

            // SAFETY: release exactly the frame count GetBuffer reported.
            unsafe { self.capture.ReleaseBuffer(frames)? };
            drained = drained.saturating_add(frames);
        }
        Ok(drained)
    }
}

impl Drop for LoopbackCapture {
    fn drop(&mut self) {
        // SAFETY: stop the stream; ignore the result during teardown.
        unsafe {
            let _ = self._client.Stop();
        };
        if self.owns_com {
            // SAFETY: balances the CoInitializeEx this instance performed.
            unsafe { CoUninitialize() };
        }
    }
}

/// Parse the shared mix format into a [`CaptureFormat`].
///
/// `pub(crate)` so the render path ([`crate::render`]) reads a render endpoint's
/// mix format with the same rules rather than duplicating the
/// `WAVEFORMATEXTENSIBLE` unpacking.
pub(crate) fn parse_format(wf: &WAVEFORMATEX) -> CaptureFormat {
    let mut is_float = wf.wFormatTag == WAVE_FORMAT_IEEE_FLOAT_TAG;
    if wf.wFormatTag as u32 == WAVE_FORMAT_EXTENSIBLE && wf.cbSize >= 22 {
        // SAFETY: an EXTENSIBLE tag with a full cbSize means the WAVEFORMATEX is
        // the head of a WAVEFORMATEXTENSIBLE; read its SubFormat by value (the
        // struct is `packed(1)`, so a reference to the field would be unaligned).
        let sub =
            unsafe { (*(wf as *const WAVEFORMATEX as *const WAVEFORMATEXTENSIBLE)).SubFormat };
        is_float = sub == SUBTYPE_IEEE_FLOAT;
    } else if wf.wFormatTag as u32 == WAVE_FORMAT_PCM {
        is_float = false;
    }
    CaptureFormat {
        sample_rate: wf.nSamplesPerSec,
        channels: wf.nChannels,
        is_float,
        bits: wf.wBitsPerSample,
    }
}

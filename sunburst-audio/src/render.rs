// SPDX-License-Identifier: GPL-2.0-or-later

//! WASAPI shared-mode render to a virtual-mic endpoint.
//!
//! The mirror of [`crate::capture::LoopbackCapture`]: where loopback *reads* a
//! render endpoint's mix, this *writes* to one. The server uses it to play the
//! pads' headset-mic audio into a consumed virtual microphone — Steam's signed
//! "Steam Streaming Microphone" render endpoint, selected by name with
//! [`crate::device::find_render_endpoint`] — so games read it as a mic input,
//! the same "consume a signed device" posture the loopback side takes with
//! "Steam Streaming Speakers". No driver of ours is installed.
//!
//! Timer-driven, not event-driven: the caller pushes decoded frames as they
//! arrive off the wire and this fits them into the shared buffer's free space,
//! dropping the overflow rather than blocking (a mic may drop; it must never
//! stall the receive thread). Underruns play as silence, which WASAPI supplies
//! on its own. Input is interleaved 48 kHz stereo i16 — the stream's format —
//! converted to the endpoint's own mix format ([`crate::pcm`]) on the way in.

use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, IAudioClient3, IAudioRenderClient,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::core::Result;

use crate::capture::{CaptureFormat, parse_format};
use crate::device::find_render_endpoint;
use crate::pcm;

/// A live WASAPI render stream to a named endpoint.
pub struct RenderPlayback {
    // Kept alive: the render client is a service of this audio client.
    client: IAudioClient3,
    render: IAudioRenderClient,
    format: CaptureFormat,
    /// The endpoint's shared-buffer capacity in frames, for the free-space sum.
    buffer_frames: u32,
    /// Scratch for one frame's converted samples, so writing does not allocate
    /// after warmup (one of the two is used, per the endpoint format).
    scratch_f32: Vec<f32>,
    scratch_i16: Vec<i16>,
    owns_com: bool,
}

impl RenderPlayback {
    /// Open shared-mode render on the active endpoint whose name contains
    /// `device_name`. `Ok(None)` when no endpoint matches — the caller then
    /// renders nowhere rather than to the wrong device. Call from the thread
    /// that will write to it; COM is initialised on that thread.
    pub fn open(device_name: &str) -> Result<Option<RenderPlayback>> {
        // SAFETY: initialise COM (multithreaded) for this thread; S_OK means we
        // own the matching CoUninitialize.
        let owns_com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).is_ok() };

        let device = match find_render_endpoint(device_name) {
            Ok(Some(d)) => d,
            Ok(None) => {
                if owns_com {
                    // SAFETY: balances the CoInitializeEx above.
                    unsafe { CoUninitialize() };
                }
                return Ok(None);
            }
            Err(e) => {
                if owns_com {
                    // SAFETY: balances the CoInitializeEx above.
                    unsafe { CoUninitialize() };
                }
                return Err(e);
            }
        };

        // SAFETY: activate the audio client, initialise shared mode against the
        // endpoint's own mix format (no loopback flag — this is a sink), take
        // the render service and start. The mix format is freed once Initialize
        // has copied it.
        unsafe {
            let client: IAudioClient3 = device.Activate(CLSCTX_ALL, None)?;
            let mix = client.GetMixFormat()?;
            let format = parse_format(&*mix);

            if format.sample_rate != 48_000 {
                log::warn!(
                    "audio: mic endpoint is {} Hz, not 48000; the pad mic is not resampled",
                    format.sample_rate
                );
            }

            // 200 ms shared buffer; periodicity 0 for a shared timer-driven sink.
            let hns_buffer = 200 * 10_000;
            let init = client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                0,
                hns_buffer,
                0,
                mix,
                None,
            );
            CoTaskMemFree(Some(mix as *const _));
            init?;

            let buffer_frames = client.GetBufferSize()?;
            let render: IAudioRenderClient = client.GetService()?;
            client.Start()?;

            Ok(Some(RenderPlayback {
                client,
                render,
                format,
                buffer_frames,
                scratch_f32: Vec::with_capacity(4096),
                scratch_i16: Vec::with_capacity(4096),
                owns_com,
            }))
        }
    }

    /// The endpoint's mix format.
    pub fn format(&self) -> CaptureFormat {
        self.format
    }

    /// Write one interleaved-stereo i16 frame, converting to the endpoint's
    /// format and channel count. Fits as many frames as the shared buffer has
    /// free and drops the rest (the mic drops rather than blocking); returns the
    /// number of source frames actually written. An unsupported endpoint bit
    /// depth writes silence, so the sink still advances.
    pub fn write_stereo(&mut self, stereo: &[i16]) -> Result<u32> {
        let frames_in = (stereo.len() / 2) as u32;
        if frames_in == 0 {
            return Ok(0);
        }

        // Free space = capacity - what is still queued.
        // SAFETY: GetCurrentPadding reports the frames still to be rendered.
        let padding = unsafe { self.client.GetCurrentPadding()? };
        let avail = self.buffer_frames.saturating_sub(padding);
        let n = frames_in.min(avail);
        if n == 0 {
            return Ok(0);
        }

        let channels = self.format.channels.max(1) as usize;
        let src = &stereo[..n as usize * 2];

        // SAFETY: GetBuffer reserves `n` frames of writable device memory; we
        // fill exactly `n * channels` samples of the endpoint's sample type and
        // release exactly `n`.
        unsafe {
            let data = self.render.GetBuffer(n)?;
            let mut flags = 0u32;
            if self.format.is_float {
                self.scratch_f32.clear();
                pcm::stereo_i16_to_f32(src, channels, &mut self.scratch_f32);
                std::ptr::copy_nonoverlapping(
                    self.scratch_f32.as_ptr(),
                    data as *mut f32,
                    self.scratch_f32.len(),
                );
            } else if self.format.bits == 16 {
                self.scratch_i16.clear();
                pcm::stereo_i16_to_i16(src, channels, &mut self.scratch_i16);
                std::ptr::copy_nonoverlapping(
                    self.scratch_i16.as_ptr(),
                    data as *mut i16,
                    self.scratch_i16.len(),
                );
            } else {
                // Unsupported depth: advance the sink with silence.
                flags = AUDCLNT_BUFFERFLAGS_SILENT.0 as u32;
            }
            self.render.ReleaseBuffer(n, flags)?;
        }
        Ok(n)
    }
}

impl Drop for RenderPlayback {
    fn drop(&mut self) {
        // SAFETY: stop the stream; ignore the result during teardown.
        unsafe {
            let _ = self.client.Stop();
        };
        if self.owns_com {
            // SAFETY: balances the CoInitializeEx this instance performed.
            unsafe { CoUninitialize() };
        }
    }
}

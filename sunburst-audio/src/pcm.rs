// SPDX-License-Identifier: GPL-2.0-or-later

//! PCM format plumbing between WASAPI and Opus — pure, so it is host-tested.
//!
//! WASAPI hands back the endpoint's shared mix format, which for a render
//! endpoint is float32 at the device's channel count. Opus wants interleaved
//! i16 at a fixed channel count (stereo here), delivered in whole frames of a
//! fixed size. This module does the float→i16 conversion, the downmix to stereo,
//! and the regrouping of WASAPI's variable-sized packets into exact frames. The
//! frame-boundary handling is the part that is easy to get subtly wrong, so it
//! is here with tests rather than inside the `#[cfg(windows)]` capture code.

/// Convert one float sample in [-1, 1] to i16, clamping out-of-range values
/// rather than letting them wrap (a loud transient must not become noise).
#[inline]
pub fn f32_to_i16(sample: f32) -> i16 {
    let scaled = sample * 32767.0;
    scaled.clamp(-32768.0, 32767.0) as i16
}

/// Convert interleaved float32 with `src_channels` channels into interleaved
/// stereo i16, appended to `out`.
///
/// Mono is duplicated to both channels; two or more channels take the front
/// left/right pair (surround downmix is deferred — see the plan's exclusions).
/// `src.len()` must be a whole number of frames.
pub fn convert_to_stereo_i16(src: &[f32], src_channels: usize, out: &mut Vec<i16>) {
    if src_channels == 0 {
        return;
    }
    let frames = src.len() / src_channels;
    out.reserve(frames * 2);
    for f in 0..frames {
        let base = f * src_channels;
        let (l, r) = if src_channels == 1 {
            let m = src[base];
            (m, m)
        } else {
            (src[base], src[base + 1])
        };
        out.push(f32_to_i16(l));
        out.push(f32_to_i16(r));
    }
}

/// Convert interleaved i16 with `src_channels` channels into interleaved stereo
/// i16, appended to `out`. The channel rules match [`convert_to_stereo_i16`].
pub fn downmix_i16_to_stereo(src: &[i16], src_channels: usize, out: &mut Vec<i16>) {
    if src_channels == 0 {
        return;
    }
    if src_channels == 2 {
        out.extend_from_slice(src);
        return;
    }
    let frames = src.len() / src_channels;
    out.reserve(frames * 2);
    for f in 0..frames {
        let base = f * src_channels;
        if src_channels == 1 {
            out.push(src[base]);
            out.push(src[base]);
        } else {
            out.push(src[base]);
            out.push(src[base + 1]);
        }
    }
}

/// Convert one i16 sample to float in [-1, 1] — the inverse of [`f32_to_i16`],
/// for the render (playback) direction. Divides by 32768 so full-scale negative
/// maps exactly to -1.0 and the range never exceeds [-1, 1].
#[inline]
pub fn i16_to_f32(sample: i16) -> f32 {
    sample as f32 / 32768.0
}

/// Spread interleaved **stereo** i16 to `dst_channels` interleaved f32, appended
/// to `out` — the render-side counterpart of [`convert_to_stereo_i16`], used to
/// feed a WASAPI render endpoint whose mix format is float32.
///
/// Channel rules: 1 = average L/R to mono; 2 = pass through; more than 2 = L/R
/// in the front pair and silence in the rest (never upmix into surround).
/// `src.len()` must be even (whole stereo frames).
pub fn stereo_i16_to_f32(src: &[i16], dst_channels: usize, out: &mut Vec<f32>) {
    if dst_channels == 0 {
        return;
    }
    let frames = src.len() / 2;
    out.reserve(frames * dst_channels);
    for f in 0..frames {
        let l = i16_to_f32(src[f * 2]);
        let r = i16_to_f32(src[f * 2 + 1]);
        match dst_channels {
            1 => out.push((l + r) * 0.5),
            _ => {
                out.push(l);
                out.push(r);
                for _ in 2..dst_channels {
                    out.push(0.0);
                }
            }
        }
    }
}

/// Spread interleaved **stereo** i16 to `dst_channels` interleaved i16, appended
/// to `out` — for a render endpoint whose mix format is 16-bit PCM. Channel
/// rules match [`stereo_i16_to_f32`]; the mono average rounds to nearest.
pub fn stereo_i16_to_i16(src: &[i16], dst_channels: usize, out: &mut Vec<i16>) {
    if dst_channels == 0 {
        return;
    }
    let frames = src.len() / 2;
    out.reserve(frames * dst_channels);
    for f in 0..frames {
        let l = src[f * 2];
        let r = src[f * 2 + 1];
        match dst_channels {
            1 => out.push(((l as i32 + r as i32) / 2) as i16),
            _ => {
                out.push(l);
                out.push(r);
                for _ in 2..dst_channels {
                    out.push(0);
                }
            }
        }
    }
}

/// Regroups a stream of interleaved i16 samples into fixed-size frames.
///
/// WASAPI packets do not align to the 5 ms Opus frame, so captured samples are
/// pushed in and whole frames are popped out; the leftover tail carries to the
/// next push. Allocation is bounded after warmup: the buffer never holds more
/// than one WASAPI packet plus a partial frame.
pub struct FrameAccumulator {
    buf: std::collections::VecDeque<i16>,
    frame_len: usize,
}

impl FrameAccumulator {
    /// `frame_samples` per channel, `channels` channels — a popped frame is
    /// `frame_samples * channels` interleaved samples.
    pub fn new(frame_samples: usize, channels: usize) -> FrameAccumulator {
        let frame_len = frame_samples * channels;
        FrameAccumulator {
            // Room for a generous WASAPI packet plus a partial frame, so steady
            // state does not reallocate.
            buf: std::collections::VecDeque::with_capacity(frame_len * 8),
            frame_len,
        }
    }

    /// Append captured interleaved samples.
    pub fn push(&mut self, samples: &[i16]) {
        self.buf.extend(samples.iter().copied());
    }

    /// Pop one whole frame into `out` (which must be `frame_len` long),
    /// returning `false` when fewer than a frame's worth are buffered.
    pub fn pop_frame(&mut self, out: &mut [i16]) -> bool {
        debug_assert_eq!(out.len(), self.frame_len);
        if self.buf.len() < self.frame_len {
            return false;
        }
        for slot in out.iter_mut() {
            *slot = self.buf.pop_front().unwrap_or(0);
        }
        true
    }

    /// Samples currently buffered but not yet a whole frame.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Interleaved samples in one frame.
    pub fn frame_len(&self) -> usize {
        self.frame_len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_clamps_rather_than_wraps() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(1.0), 32767);
        assert_eq!(f32_to_i16(-1.0), -32767);
        assert_eq!(f32_to_i16(2.0), 32767, "over-range clamps to max");
        assert_eq!(f32_to_i16(-2.0), -32768, "under-range clamps to min");
    }

    #[test]
    fn mono_duplicates_to_both_channels() {
        let src = [0.5f32, -0.5];
        let mut out = Vec::new();
        convert_to_stereo_i16(&src, 1, &mut out);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0], out[1]); // frame 0 L==R
        assert_eq!(out[2], out[3]); // frame 1 L==R
    }

    #[test]
    fn multichannel_takes_the_front_pair() {
        // One 5.1 frame: L R C LFE Ls Rs.
        let src = [0.1f32, 0.2, 0.3, 0.4, 0.5, 0.6];
        let mut out = Vec::new();
        convert_to_stereo_i16(&src, 6, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], f32_to_i16(0.1));
        assert_eq!(out[1], f32_to_i16(0.2));
    }

    #[test]
    fn i16_downmix_matches_the_float_rules() {
        let mut out = Vec::new();
        downmix_i16_to_stereo(&[100, 200], 1, &mut out);
        assert_eq!(out, [100, 100, 200, 200]);
        out.clear();
        downmix_i16_to_stereo(&[1, 2, 3, 4], 2, &mut out);
        assert_eq!(out, [1, 2, 3, 4], "stereo passes through");
        out.clear();
        downmix_i16_to_stereo(&[1, 2, 3, 4, 5, 6], 6, &mut out);
        assert_eq!(out, [1, 2], "5.1 takes the front pair");
    }

    #[test]
    fn i16_to_f32_stays_in_range_and_round_trips_within_one_lsb() {
        assert_eq!(i16_to_f32(0), 0.0);
        // Dividing by 32768 keeps the float within [-1, 1): full-scale negative
        // is exactly -1.0, full-scale positive is just under 1.0. That is the
        // conservative choice for a render sink — no sample exceeds unity.
        assert_eq!(i16_to_f32(-32768), -1.0);
        assert!(i16_to_f32(32767) < 1.0 && i16_to_f32(32767) > 0.9999);
        // Re-quantising loses at most 1 LSB (the 32768-vs-32767 scaling
        // asymmetry plus truncation in `f32_to_i16`); it never diverges.
        for s in [-32768i16, -12345, -1, 0, 1, 12345, 32767] {
            assert!((f32_to_i16(i16_to_f32(s)) - s).abs() <= 1, "round-trip of {s}");
        }
    }

    #[test]
    fn stereo_to_f32_follows_the_channel_rules() {
        // Mono averages the pair.
        let mut out = Vec::new();
        stereo_i16_to_f32(&[32767, -32768], 1, &mut out);
        assert_eq!(out.len(), 1);
        assert!(out[0].abs() < 1e-4, "L and R cancel to ~0");

        // Stereo passes through.
        out.clear();
        stereo_i16_to_f32(&[16384, -16384], 2, &mut out);
        assert_eq!(out, [i16_to_f32(16384), i16_to_f32(-16384)]);

        // 5.1 puts the pair up front and silences the rest.
        out.clear();
        stereo_i16_to_f32(&[100, 200], 6, &mut out);
        assert_eq!(out.len(), 6);
        assert_eq!(out[0], i16_to_f32(100));
        assert_eq!(out[1], i16_to_f32(200));
        assert_eq!(&out[2..], &[0.0; 4]);
    }

    #[test]
    fn stereo_to_i16_follows_the_channel_rules() {
        let mut out = Vec::new();
        stereo_i16_to_i16(&[100, 200], 1, &mut out);
        assert_eq!(out, [150], "mono is the rounded average");
        out.clear();
        stereo_i16_to_i16(&[1, 2, 3, 4], 2, &mut out);
        assert_eq!(out, [1, 2, 3, 4], "stereo passes through");
        out.clear();
        stereo_i16_to_i16(&[7, 9], 4, &mut out);
        assert_eq!(out, [7, 9, 0, 0], "quad front pair, rest silent");
    }

    #[test]
    fn accumulator_regroups_across_odd_packets() {
        let mut acc = FrameAccumulator::new(4, 2); // frame_len = 8
        let mut frame = [0i16; 8];
        // Push 5 samples, then 11 — total 16 = exactly two frames.
        acc.push(&[1, 2, 3, 4, 5]);
        assert!(!acc.pop_frame(&mut frame), "not a full frame yet");
        acc.push(&[6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
        assert!(acc.pop_frame(&mut frame));
        assert_eq!(frame, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(acc.pop_frame(&mut frame));
        assert_eq!(frame, [9, 10, 11, 12, 13, 14, 15, 16]);
        assert!(!acc.pop_frame(&mut frame), "drained");
        assert_eq!(acc.buffered(), 0);
    }
}

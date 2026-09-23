// SPDX-License-Identifier: GPL-2.0-or-later

//! Prove the capture→convert→encode spine end to end: grab the monitor, convert
//! scRGB→P010, encode HEVC with subframe slices, write a raw `.265` file.
//!
//! Box-only — it needs a GPU, a desktop, and NVENC. Run on the 4070:
//! `cargo run --example capture_encode -- 300` (300 frames). Play the result
//! with `ffplay capture.265`. This host has none of that, so the example only
//! compiles here; `cargo xwin build --example capture_encode` type-checks it.

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    use std::time::{Duration, Instant};

    use sunburst_capture::{Frame, OutputSelect, select};
    use sunburst_encode::convert::{ConvertOutput, Converter};
    use sunburst_encode::encoder::{Codec, Encoder, EncoderConfig, PicRequest};
    use sunburst_encode::nvenc::Nvenc;
    use windows::Win32::Graphics::Direct3D11::ID3D11Device;
    use windows::core::Interface;

    let frame_count: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);
    // Second arg picks the codec: `av1` → AV1, anything else → HEVC.
    let (codec, path) = match std::env::args().nth(2).as_deref() {
        Some("av1") => (Codec::Av1, "capture.obu"),
        Some("h264") => (Codec::H264, "capture.264"),
        _ => (Codec::Hevc, "capture.265"),
    };
    let slices: u32 = 4; // 4 slices, or a 2×2 AV1 tile grid

    // hdr-preferred D3D11 capture (NvFBC off), the runtime NVENC, an output file.
    let mut capture = select::build(false, None, true, OutputSelect::Primary)?;
    let hdr = capture.caps().hdr_metadata; // the display's mastering metadata, if HDR
    let nvenc = Nvenc::load()?;
    let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);

    let mut converter: Option<Converter> = None;
    let mut encoder: Option<Encoder> = None;
    let mut written = 0u32;
    let mut bytes = 0usize;
    let started = Instant::now();

    while written < frame_count {
        // Only the D3D11 backends are wired here; the NvFBC (CUDA) path encodes
        // through a CUDA kernel + NVENC-CUDA session, a separate example.
        let Some(Frame::Texture(tf)) = capture.acquire(Duration::from_millis(100))? else {
            continue;
        };
        let (w, h) = (tf.meta.width, tf.meta.height);

        // scRGB FP16 → P010 (HEVC/AV1) or NV12 (H.264), on the texture's device.
        let output = if codec == Codec::H264 {
            ConvertOutput::Nv12
        } else {
            ConvertOutput::P010
        };
        let conv = match &mut converter {
            Some(c) => c,
            None => converter.insert(Converter::new(
                &tf.texture,
                output,
                hdr.is_some(),
                matches!(tf.format, sunburst_capture::TextureFormat::Bgra8),
            )?),
        };
        let p010 = conv.convert(&tf.texture, w, h)?;
        let p010_raw = p010.as_raw();

        // Encode against that same device; build the encoder on the first frame.
        let enc = match &mut encoder {
            Some(e) => e,
            None => {
                // SAFETY: a captured texture always has a live device.
                let device: ID3D11Device = unsafe { tf.texture.GetDevice() }?;
                let mut ecfg = EncoderConfig::new(codec, w, h);
                ecfg.slices = slices;
                ecfg.hdr = hdr;
                encoder.insert(Encoder::new(&nvenc, device.as_raw(), &ecfg)?)
            }
        };
        // A keyframe on the first frame; the rest predict from it.
        let req = PicRequest {
            timestamp: written as u64,
            force_idr: written == 0,
        };
        enc.encode_slices(p010_raw, req, |slice, _is_idr| {
            bytes += slice.len();
            let _ = file.write_all(slice);
        })?;
        written += 1;
    }

    file.flush()?;
    let secs = started.elapsed().as_secs_f64();
    println!(
        "wrote {written} frames, {bytes} bytes to capture.265 in {secs:.1}s ({:.1} fps)",
        f64::from(written) / secs
    );
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("capture_encode runs on Windows only (capture + NVENC)");
}

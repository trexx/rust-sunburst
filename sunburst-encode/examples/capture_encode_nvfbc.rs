// SPDX-License-Identifier: GPL-2.0-or-later

//! The NvFBC CUDA-native encode path, end to end: NvFBC grabs the desktop to a
//! CUDA ARGB10 buffer, a CUDA kernel converts it to P010, and an NVENC-CUDA
//! session encodes it — no D3D11, the frame never leaves the GPU.
//!
//! Box-only, and NvFBC is opt-in resilience: needs the keyed NvFBC create + a
//! CUDA-toolkit-compiled PTX vendored over the placeholder (see
//! `.github/workflows/cuda-kernel.yml`). Run on the 4070:
//! `cargo run --example capture_encode_nvfbc -- 300 [av1]`. Type-checks here.

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::ffi::c_void;
    use std::io::Write;

    use sunburst_capture::tocuda::NvFbcCapture;
    use sunburst_capture::{Capture, Frame};
    use sunburst_encode::cuda_convert::CudaConverter;
    use sunburst_encode::encoder::{Codec, Encoder};
    use sunburst_encode::nvenc::Nvenc;

    let frame_count: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);
    let (codec, path) = match std::env::args().nth(2).as_deref() {
        Some("av1") => (Codec::Av1, "capture_nvfbc.obu"),
        _ => (Codec::Hevc, "capture_nvfbc.265"),
    };
    let slices: u32 = if codec == Codec::Av1 { 2 } else { 4 };

    let mut capture = NvFbcCapture::new(true)?; // request 10-bit / HDR
    let context = capture.context(); // NvFBC's CUDA context — shared downstream
    let nvenc = Nvenc::load()?;
    let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);

    let mut converter: Option<CudaConverter> = None;
    let mut encoder: Option<Encoder> = None;
    let mut written = 0u32;
    let mut bytes = 0usize;

    while written < frame_count {
        let Some(Frame::Cuda(cf)) = capture.acquire(std::time::Duration::from_millis(100))? else {
            continue;
        };
        let (w, h) = (cf.meta.width, cf.meta.height);

        // ARGB10 → P010, in NvFBC's context.
        let conv = match &mut converter {
            Some(c) => c,
            None => converter.insert(CudaConverter::new(context, w, h)?),
        };
        let p010 = conv.convert(cf.device_ptr, cf.pitch as u32)?;

        // NVENC-CUDA session on the same context; the P010 pitch comes from the
        // converter. NvFBC's caps carry no HDR metadata yet, so `None`.
        let enc = match &mut encoder {
            Some(e) => e,
            None => encoder.insert(Encoder::new_cuda(
                &nvenc,
                context,
                w,
                h,
                codec,
                slices,
                None,
                conv.pitch(),
            )?),
        };
        enc.encode_slices(p010 as *mut c_void, |slice| {
            bytes += slice.len();
            let _ = file.write_all(slice);
        })?;
        written += 1;
    }

    file.flush()?;
    println!("wrote {written} frames, {bytes} bytes to {path}");
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("capture_encode_nvfbc runs on Windows only (NvFBC + CUDA + NVENC)");
}

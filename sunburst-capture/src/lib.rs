// SPDX-License-Identifier: GPL-2.0-or-later
#![cfg(windows)]

//! Screen capture: the [`Capture`] trait and its DDA / WGC / NvFBC backends.
//!
//! Windows-only. On any other host this crate compiles to nothing, so
//! `cargo check` stays green on the Linux development machine while
//! `cargo xwin check --target x86_64-pc-windows-msvc` checks the real thing.
//! Neither command lies about the other.
//!
//! # One trait, two surface kinds
//!
//! Every backend captures the **monitor**, never a window (CLAUDE.md), and hands
//! back a [`Frame`]. But the frame is *tagged by how it was produced*, because
//! the backends do not share a runtime and each should take its shortest path to
//! the encoder:
//!
//! - **DDA** and **WGC** produce a `D3D11` texture natively — zero copy. They
//!   yield [`Frame::Texture`], which feeds the HLSL colour-convert shader and
//!   NVENC's DirectX input.
//! - **NvFBC** grabs into a CUDA device pointer. It yields [`Frame::Cuda`], which
//!   feeds a CUDA convert kernel and NVENC's `CUDADEVICEPTR` input — it is **not**
//!   bounced through D3D11. DDA/WGC and NvFBC are mutually exclusive at runtime,
//!   so unifying them onto one D3D11 path would only add a device-to-device copy
//!   on the opt-in resilience backend for no shared-path benefit. (This revises
//!   CLAUDE.md's original "the trait yields a D3D11 texture whatever produced it";
//!   see `memory/capture-path-nvfbc-cuda-native.md`.)
//!
//! # Rebuild is normal, and hot-swap rides the same path
//!
//! [`CaptureError::AccessLost`] happens constantly — mode changes, fullscreen
//! transitions, desktop switches — and the owner must be able to drop the backend
//! and build a fresh one at any moment. Switching *which* backend is running mid
//! stream is the same operation: drop the old [`Capture`], construct the new one,
//! and have the encoder emit one IDR. There is no in-trait `switch`; the owner
//! (the encode loop) holds a `Box<dyn Capture>` and replaces it, treating a
//! requested swap and an `AccessLost` identically.

use std::time::Duration;

use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

pub mod dda;
pub mod wgc;

// NvFBC (opt-in resilience). `cuda`/`nvfbc` are the internal FFI; `tocuda`
// exposes the backend.
pub mod cuda;
mod nvfbc;
pub mod select;
pub mod tocuda;

/// Which capture backend produced (or would produce) a frame.
///
/// WGC is the Win11 default and DDA the Win10 default, each falling back to the
/// other; **NvFBC is never a default** — it is opt-in resilience only (DRM /
/// secure-desktop coverage), measured 1.2–1.5 ms slower than DDA.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    /// Desktop Duplication (DXGI). Win10 default.
    Dda,
    /// Windows.Graphics.Capture. Win11 default.
    Wgc,
    /// NvFBC → CUDA. Opt-in only.
    NvFbc,
}

/// Per-frame metadata carried alongside whichever surface a backend produced.
#[derive(Clone, Copy, Debug)]
pub struct FrameMeta {
    pub width: u32,
    pub height: u32,
    /// The source was HDR (scRGB FP16 / BT.2020 PQ), so the convert stage must
    /// tone-map rather than assume SDR.
    pub hdr: bool,
    /// QPC ticks at the frame's present time, for glass-to-glass accounting.
    pub present_qpc: i64,
}

/// The pixel format of a [`TextureFrame`] — what the HLSL convert shader binds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextureFormat {
    /// `DXGI_FORMAT_B8G8R8A8_UNORM` — SDR desktop.
    Bgra8,
    /// `DXGI_FORMAT_R16G16B16A16_FLOAT` — scRGB linear FP16, the HDR desktop.
    Rgba16Float,
}

/// The pixel format of a [`CudaFrame`]. NvFBCToCuda offers only these two — it
/// does **not** hand back FP16 — so the P010 CUDA convert kernel keys off this.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CudaFormat {
    /// `NVFBC_TOCUDA_ARGB` — 8-bit BGRA, SDR.
    Argb8,
    /// `NVFBC_TOCUDA_ARGB10` — A2B10G10R10, a 10-bit *integer* format (not FP16).
    Argb10,
}

/// A D3D11-backed frame — what DDA and WGC produce.
pub struct TextureFrame {
    /// The captured texture. The colour-convert shader reads it directly.
    pub texture: ID3D11Texture2D,
    /// The texture's pixel format, so the convert stage binds the right shader.
    pub format: TextureFormat,
    pub meta: FrameMeta,
}

/// A CUDA-backed frame — what NvFBC produces. The pointer is owned by the NvFBC
/// session for the life of the grab; the convert kernel reads it in place.
pub struct CudaFrame {
    /// `CUdeviceptr` to the captured surface (kept as a raw integer so the trait
    /// surface does not pull in the CUDA FFI types).
    pub device_ptr: u64,
    /// Row pitch in bytes.
    pub pitch: usize,
    /// The surface's pixel format (NvFBCToCuda is ARGB8 or ARGB10, never FP16).
    pub format: CudaFormat,
    pub meta: FrameMeta,
}

/// A captured frame, tagged by the surface kind so the encoder takes each
/// backend's shortest path. See the module docs.
pub enum Frame {
    Texture(TextureFrame),
    Cuda(CudaFrame),
}

impl Frame {
    /// The frame's metadata, whichever surface kind it is.
    pub fn meta(&self) -> &FrameMeta {
        match self {
            Frame::Texture(f) => &f.meta,
            Frame::Cuda(f) => &f.meta,
        }
    }
}

/// What a backend can do, queried once after it is built.
#[derive(Clone, Copy, Debug)]
pub struct Caps {
    pub backend: Backend,
    /// The output is in HDR mode, so frames arrive as scRGB FP16.
    pub hdr: bool,
    pub width: u32,
    pub height: u32,
    /// The display's HDR mastering metadata, when the output is HDR and the
    /// backend could read it — forwarded to the encoder for the mastering-display
    /// / content-light-level SEI/OBU.
    pub hdr_metadata: Option<HdrMetadata>,
}

/// The display's HDR mastering metadata, read from the DXGI output desc. CIE xy
/// chromaticity for the primaries + white point, luminance in nits.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HdrMetadata {
    pub red: [f32; 2],
    pub green: [f32; 2],
    pub blue: [f32; 2],
    pub white: [f32; 2],
    /// Display minimum luminance (nits).
    pub min_luminance: f32,
    /// Display maximum luminance (nits) — also the MaxCLL first cut.
    pub max_luminance: f32,
    /// Maximum full-frame-average luminance (nits) — MaxFALL.
    pub max_full_frame_luminance: f32,
}

/// Why a capture attempt failed.
#[derive(Debug)]
pub enum CaptureError {
    /// The capture object is stale and must be rebuilt — a mode change,
    /// fullscreen transition, or desktop switch. Recoverable and *expected*; the
    /// owner drops and reconstructs the backend. Never fatal.
    AccessLost,
    /// The desktop cannot be captured right now — the secure desktop (UAC / lock
    /// screen) or DRM-protected content. The pipeline must send a placeholder
    /// frame, never a frozen one.
    Unavailable,
    /// A backend-specific fatal error that a rebuild will not fix (bad device,
    /// missing NvFBC key). Carries a message.
    Backend(String),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::AccessLost => write!(f, "capture access lost — rebuild"),
            CaptureError::Unavailable => write!(f, "desktop unavailable — placeholder"),
            CaptureError::Backend(m) => write!(f, "capture backend error: {m}"),
        }
    }
}

impl std::error::Error for CaptureError {}

/// A monitor capture source. One backend, one output.
///
/// Implementations must be safe to drop and reconstruct at any time (see
/// [`CaptureError::AccessLost`]). `acquire` blocks until a frame is ready or a
/// bounded wait elapses, depending on the backend; the owning loop is a dedicated
/// OS thread per CLAUDE.md's hot-path rules.
pub trait Capture {
    /// Block up to `timeout` for the next frame.
    ///
    /// The returned frame's surface is **owned by the backend and valid only
    /// until the next `acquire`** — DDA invalidates it at `ReleaseFrame`, WGC
    /// recycles the pool slot, NvFBC reuses the grab buffer. The caller reads it
    /// (the colour-convert stage) before acquiring again; no copy is taken here.
    ///
    /// - `Ok(Some(frame))` — a new frame is ready.
    /// - `Ok(None)` — nothing new within `timeout` (an idle desktop presents
    ///   nothing); the caller reuses the last frame rather than treating it as an
    ///   error. DDA's `WAIT_TIMEOUT` and WGC's empty `TryGetNextFrame` land here.
    /// - `Err(AccessLost)` — rebuild the backend.
    /// - `Err(Unavailable)` — secure desktop / DRM; send a placeholder and retry.
    /// - `Err(Backend)` — fatal for this backend; the owner falls back.
    fn acquire(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError>;

    /// This backend's static capabilities.
    fn caps(&self) -> Caps;
}

/// Choose the default backend for the host.
///
/// WGC on Win11, DDA on Win10, and NvFBC only when explicitly opted in — it is
/// resilience, not speed. The cross-fallback (WGC↔DDA on a build where one
/// refuses) is the owner's job when [`Capture::acquire`] returns
/// `Backend(_)` at construction; this only picks the first choice.
pub fn default_backend(is_win11: bool, nvfbc_opt_in: bool) -> Backend {
    if nvfbc_opt_in {
        Backend::NvFbc
    } else if is_win11 {
        Backend::Wgc
    } else {
        Backend::Dda
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvfbc_is_never_a_default() {
        // Opt-in beats OS default, but is the only way to NvFBC.
        assert_eq!(default_backend(true, false), Backend::Wgc);
        assert_eq!(default_backend(false, false), Backend::Dda);
        assert_eq!(default_backend(true, true), Backend::NvFbc);
        assert_eq!(default_backend(false, true), Backend::NvFbc);
    }

    #[test]
    fn frame_meta_is_reachable_through_either_surface() {
        let meta = FrameMeta {
            width: 3840,
            height: 2160,
            hdr: true,
            present_qpc: 42,
        };
        let cuda = Frame::Cuda(CudaFrame {
            device_ptr: 0,
            pitch: 3840 * 8,
            format: CudaFormat::Argb10,
            meta,
        });
        assert_eq!(cuda.meta().width, 3840);
        assert!(cuda.meta().hdr);
    }
}

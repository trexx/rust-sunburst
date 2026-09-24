// SPDX-License-Identifier: GPL-2.0-or-later

//! ARGB10 → P010 colour convert on the GPU, for the NvFBC (CUDA) path.
//!
//! NvFBC grabs the desktop into an ARGB10 CUDA buffer; this launches the
//! [`argb10_to_p010`](../cuda/argb10_to_p010.cu) kernel to produce a P010 CUDA
//! buffer the NVENC-CUDA session encodes directly — the frame never leaves the
//! GPU or crosses into D3D11. It runs in **NvFBC's own CUDA context** (adopted via
//! `cuCtxPushCurrent`), where the grab buffer lives.
//!
//! The kernel is embedded as PTX (`include_bytes!`) and JIT-compiled by the driver
//! at load, so no CUDA toolkit is needed at runtime. The PTX is vendored from
//! `.github/workflows/cuda-kernel.yml`, never written by hand;
//! `tests/ptx_vendored.rs` checks it exports the entries named below.

use std::ffi::c_void;

use sunburst_capture::cuda::{CUDA_SUCCESS, CuContext, CuDevicePtr, CuFunction, Cuda};

use crate::convert::ConvertOutput;

/// The compiled convert kernels, vendored from CI; see the module docs.
const KERNEL_PTX_P010: &[u8] = include_bytes!("../cuda/argb10_to_p010.ptx");
const KERNEL_PTX_NV12: &[u8] = include_bytes!("../cuda/argb_to_nv12.ptx");

/// Converts ARGB10 CUDA buffers to P010 (HEVC/AV1) or NV12 (H.264 SDR), in a
/// shared CUDA context.
pub struct CudaConverter {
    cuda: Cuda,
    kernel: CuFunction,
    /// The output buffer (P010 or NV12), reused each frame.
    out_buf: CuDevicePtr,
    /// Output row pitch in bytes — pass to [`Encoder::new_cuda`](crate::encoder::Encoder::new_cuda).
    pitch_bytes: u32,
    /// Destination stride in *elements* (u16 for P010, u8 for NV12).
    dst_pitch_elems: i32,
    /// The kernel's name, for the launch-failure message.
    kernel_name: &'static str,
    width: u32,
    height: u32,
}

impl CudaConverter {
    /// Build a converter in NvFBC's CUDA `context`, allocating an `output` buffer
    /// (P010 for HEVC/AV1, NV12 for H.264) for `width`×`height`. `hdr_source`
    /// selects the tonemapping NV12 kernel for an HDR desktop; it is ignored by
    /// the P010 kernel.
    pub fn new(
        context: CuContext,
        width: u32,
        height: u32,
        output: ConvertOutput,
        hdr_source: bool,
    ) -> Result<CudaConverter, String> {
        let cuda = Cuda::load().map_err(|e| format!("cuda load: {e}"))?;
        // NvFBC already called cuInit; repeating it is harmless.
        if cuda.init() != CUDA_SUCCESS {
            return Err("cuInit failed".into());
        }
        // Adopt NvFBC's context for the allocation + kernel launches.
        if cuda.ctx_push(context) != CUDA_SUCCESS {
            return Err("cuCtxPushCurrent failed".into());
        }

        // Pick the kernel, its output pitch/size, and destination element stride.
        let (ptx_src, kernel_name, pitch_bytes, bytes) = match output {
            ConvertOutput::P010 | ConvertOutput::P010Sdr => (
                KERNEL_PTX_P010,
                "argb10_to_p010",
                width * 2,
                // Y plane (w·h·2) + interleaved UV (w·h) = w·h·3 bytes.
                (width as usize) * (height as usize) * 3,
            ),
            ConvertOutput::Nv12 => (
                KERNEL_PTX_NV12,
                if hdr_source {
                    "argb_to_nv12_tonemap"
                } else {
                    "argb_to_nv12"
                },
                width,
                // Y plane (w·h) + interleaved UV (w·h/2) = w·h·3/2 bytes.
                (width as usize) * (height as usize) * 3 / 2,
            ),
        };
        // Elements: u16 for P010 (pitch/2), u8 for NV12 (pitch).
        let dst_pitch_elems = match output {
            ConvertOutput::P010 | ConvertOutput::P010Sdr => (pitch_bytes / 2) as i32,
            ConvertOutput::Nv12 => pitch_bytes as i32,
        };

        // cuModuleLoadData wants NUL-terminated PTX text.
        let mut ptx = ptx_src.to_vec();
        ptx.push(0);
        let module = cuda
            .module_load_data(&ptx)
            .map_err(|s| format!("cuModuleLoadData: {s}"))?;
        // The module stays loaded in the context for the process's life; the
        // kernel handle keeps working without holding the module handle.
        let kernel = cuda
            .module_get_function(module, kernel_name)
            .map_err(|s| format!("cuModuleGetFunction: {s}"))?;

        let out_buf = cuda
            .mem_alloc(bytes)
            .map_err(|s| format!("cuMemAlloc({bytes}): {s}"))?;

        Ok(CudaConverter {
            cuda,
            kernel,
            out_buf,
            pitch_bytes,
            dst_pitch_elems,
            kernel_name,
            width,
            height,
        })
    }

    /// Convert one ARGB10 device buffer to the output format (P010/NV12).
    /// `src_pitch` is the ARGB10 row pitch (bytes). Returns the output device
    /// pointer, valid until the next call.
    pub fn convert(&self, argb10: CuDevicePtr, src_pitch: u32) -> Result<CuDevicePtr, String> {
        // Kernel args, held in locals so `params` can point at each.
        let mut src = argb10;
        let mut src_pitch_words = (src_pitch / 4) as i32;
        let mut dst = self.out_buf;
        let mut dst_pitch_elems = self.dst_pitch_elems;
        let mut width = self.width as i32;
        let mut height = self.height as i32;
        let mut params: [*mut c_void; 6] = [
            std::ptr::addr_of_mut!(src).cast(),
            std::ptr::addr_of_mut!(src_pitch_words).cast(),
            std::ptr::addr_of_mut!(dst).cast(),
            std::ptr::addr_of_mut!(dst_pitch_elems).cast(),
            std::ptr::addr_of_mut!(width).cast(),
            std::ptr::addr_of_mut!(height).cast(),
        ];

        // One thread per 2×2 block (one chroma sample), 16×16 threads per group.
        let block = (16u32, 16u32, 1u32);
        let grid = (
            (self.width / 2).div_ceil(block.0),
            (self.height / 2).div_ceil(block.1),
            1,
        );
        if self
            .cuda
            .launch_kernel(self.kernel, grid, block, &mut params)
            != CUDA_SUCCESS
        {
            return Err(format!("cuLaunchKernel({}) failed", self.kernel_name));
        }
        if self.cuda.ctx_synchronize() != CUDA_SUCCESS {
            return Err("cuCtxSynchronize failed".into());
        }
        Ok(self.out_buf)
    }

    /// The P010 output's row pitch (bytes).
    pub fn pitch(&self) -> u32 {
        self.pitch_bytes
    }
}

impl Drop for CudaConverter {
    fn drop(&mut self) {
        if self.out_buf != 0 {
            self.cuda.mem_free(self.out_buf);
            self.out_buf = 0;
        }
        // The module is left loaded (process-lifetime); no cuModuleUnload wired.
    }
}

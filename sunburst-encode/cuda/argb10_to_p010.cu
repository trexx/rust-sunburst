// SPDX-License-Identifier: GPL-2.0-or-later
//
// ARGB10 (A2B10G10R10, one u32 per pixel — NvFBCToCuda's 10-bit output) -> P010
// (10-bit YUV 4:2:0), BT.2020 non-constant luminance, limited (studio) range.
//
// The input is assumed to already be BT.2020 PQ 10-bit — NvFBC captures the HDR
// desktop as scanned out to the display, which is the PQ signal — so this applies
// no transfer function, only RGB->YCbCr. (The D3D11 path's HLSL shader, by
// contrast, gets scRGB linear FP16 and must apply PQ itself.) This assumption is
// the one thing to confirm on the 4070.
//
// Compiled to PTX by .github/workflows/cuda-kernel.yml (nvcc -ptx) and embedded in
// sunburst-encode; the driver JITs the PTX, so no CUDA toolkit is needed at
// runtime. One thread owns a 2x2 block (one chroma sample).

#include <cstdint>

__device__ __forceinline__ void unpack(uint32_t px, float& r, float& g, float& b) {
    // A2B10G10R10: R = bits 0-9, G = 10-19, B = 20-29, A = 30-31.
    r = (float)(px & 0x3FF);
    g = (float)((px >> 10) & 0x3FF);
    b = (float)((px >> 20) & 0x3FF);
}

__device__ __forceinline__ void rgb_to_ycbcr(float r, float g, float b,
                                              float& y, float& cb, float& cr) {
    // 10-bit code inputs (0..1023) -> limited-range 10-bit YCbCr codes.
    float yn  = (0.2627f * r + 0.6780f * g + 0.0593f * b) / 1023.0f; // 0..1
    float cbn = ((b / 1023.0f) - yn) / 1.8814f;
    float crn = ((r / 1023.0f) - yn) / 1.4746f;
    y  = 64.0f  + yn  * 876.0f;
    cb = 512.0f + cbn * 896.0f;
    cr = 512.0f + crn * 896.0f;
}

__device__ __forceinline__ uint16_t p010(float code10) {
    if (code10 < 0.0f) code10 = 0.0f;
    if (code10 > 1023.0f) code10 = 1023.0f;
    // 10-bit code left-justified into 16 bits (P010 = code << 6).
    return (uint16_t)(((uint32_t)(code10 + 0.5f)) << 6);
}

// src_pitch_words / dst_pitch_elems are strides in elements (u32 / u16).
extern "C" __global__ void argb10_to_p010(const uint32_t* src, int src_pitch_words,
                                          uint16_t* dst, int dst_pitch_elems,
                                          int width, int height) {
    int bx = blockIdx.x * blockDim.x + threadIdx.x; // chroma column
    int by = blockIdx.y * blockDim.y + threadIdx.y; // chroma row
    int x = bx * 2, y = by * 2;
    if (x >= width || y >= height) return;

    uint16_t* dst_y = dst;
    uint16_t* dst_uv = dst + (size_t)height * dst_pitch_elems;

    float cb_sum = 0.0f, cr_sum = 0.0f;
    int n = 0;
    for (int dy = 0; dy < 2; ++dy) {
        for (int dx = 0; dx < 2; ++dx) {
            int px = x + dx, py = y + dy;
            if (px >= width || py >= height) continue;
            float r, g, b, yv, cb, cr;
            unpack(src[(size_t)py * src_pitch_words + px], r, g, b);
            rgb_to_ycbcr(r, g, b, yv, cb, cr);
            dst_y[(size_t)py * dst_pitch_elems + px] = p010(yv);
            cb_sum += cb;
            cr_sum += cr;
            ++n;
        }
    }
    float inv = 1.0f / (float)n;
    dst_uv[(size_t)by * dst_pitch_elems + bx * 2 + 0] = p010(cb_sum * inv);
    dst_uv[(size_t)by * dst_pitch_elems + bx * 2 + 1] = p010(cr_sum * inv);
}

// SPDX-License-Identifier: GPL-2.0-or-later
//
// ARGB10 (A2B10G10R10, one u32 per pixel — NvFBCToCuda's 10-bit output) -> NV12
// (8-bit YUV 4:2:0), Rec.709 non-constant luminance, limited (studio) range —
// the H.264 SDR path for the NvFBC backend, the CUDA counterpart of the D3D11
// SDR HLSL shader.
//
// Two entry points, chosen by whether the captured desktop is HDR:
//   argb_to_nv12          — SDR desktop: the 10-bit codes are already Rec.709
//                           gamma-encoded, so normalise and go straight to YCbCr.
//   argb_to_nv12_tonemap  — HDR desktop: NvFBC scans out the PQ BT.2020 signal,
//                           so PQ-decode -> BT.2020 linear -> Rec.709 primaries ->
//                           ACES tonemap -> Rec.709 OETF -> YCbCr.
//
// Compiled to PTX by .github/workflows/cuda-kernel.yml (nvcc -ptx) and embedded;
// the checked-in .ptx is a no-op placeholder. Like the P010 kernel, the exact
// transfer/tonemap is the thing to confirm on the 4070. One thread owns a 2x2
// block (one chroma sample).

#include <cstdint>

__device__ __forceinline__ void unpack(uint32_t px, float& r, float& g, float& b) {
    // A2B10G10R10: R = bits 0-9, G = 10-19, B = 20-29 — 10-bit codes 0..1023.
    r = (float)(px & 0x3FF);
    g = (float)((px >> 10) & 0x3FF);
    b = (float)((px >> 20) & 0x3FF);
}

__device__ __forceinline__ float pq_decode(float e) {  // e in [0,1] -> linear [0,1] (1==10000 nits)
    const float m1 = 0.1593017578125f, m2 = 78.84375f;
    const float c1 = 0.8359375f, c2 = 18.8515625f, c3 = 18.6875f;
    float ep = powf(fmaxf(e, 0.0f), 1.0f / m2);
    float num = fmaxf(ep - c1, 0.0f);
    return powf(num / (c2 - c3 * ep), 1.0f / m1);
}

__device__ __forceinline__ float aces(float x) {
    const float a = 2.51f, b = 0.03f, c = 2.43f, d = 0.59f, e = 0.14f;
    float y = (x * (a * x + b)) / (x * (c * x + d) + e);
    return fminf(fmaxf(y, 0.0f), 1.0f);
}

__device__ __forceinline__ float bt709_oetf(float c) {
    c = fminf(fmaxf(c, 0.0f), 1.0f);
    return c < 0.018f ? 4.5f * c : 1.099f * powf(c, 0.45f) - 0.099f;
}

// Rec.709 gamma-encoded R'G'B' in [0,1] -> limited-range 8-bit YCbCr codes.
__device__ __forceinline__ void ycbcr709(float r, float g, float b,
                                          float& y, float& cb, float& cr) {
    float yn = 0.2126f * r + 0.7152f * g + 0.0722f * b;
    y  = 16.0f  + yn * 219.0f;
    cb = 128.0f + ((b - yn) / 1.8556f) * 224.0f;
    cr = 128.0f + ((r - yn) / 1.5748f) * 224.0f;
}

__device__ __forceinline__ uint8_t clamp8(float code) {
    if (code < 0.0f) code = 0.0f;
    if (code > 255.0f) code = 255.0f;
    return (uint8_t)(code + 0.5f);
}

// Map one pixel's 10-bit codes to Rec.709 gamma-encoded R'G'B' [0,1].
__device__ __forceinline__ void to_rec709(float r10, float g10, float b10, bool tonemap,
                                           float& r, float& g, float& b) {
    if (!tonemap) {
        // SDR desktop: already Rec.709 gamma-encoded.
        r = r10 / 1023.0f; g = g10 / 1023.0f; b = b10 / 1023.0f;
        return;
    }
    // HDR desktop: PQ BT.2020 -> linear -> Rec.709 primaries -> ACES -> OETF.
    float lr = pq_decode(r10 / 1023.0f);
    float lg = pq_decode(g10 / 1023.0f);
    float lb = pq_decode(b10 / 1023.0f);
    // BT.2020 -> Rec.709 linear primaries.
    float r709 =  1.6605f * lr - 0.5876f * lg - 0.0728f * lb;
    float g709 = -0.1246f * lr + 1.1329f * lg - 0.0083f * lb;
    float b709 = -0.0182f * lr - 0.1006f * lg + 1.1187f * lb;
    // PQ linear is 1.0 == 10000 nits; scale so ~SDR white (100 nits) sits near 1.
    const float scale = 100.0f;
    r = bt709_oetf(aces(r709 * scale));
    g = bt709_oetf(aces(g709 * scale));
    b = bt709_oetf(aces(b709 * scale));
}

__device__ __forceinline__ void convert(const uint32_t* src, int src_pitch_words,
                                         uint8_t* dst, int dst_pitch_elems,
                                         int width, int height, bool tonemap) {
    int bx = blockIdx.x * blockDim.x + threadIdx.x; // chroma column
    int by = blockIdx.y * blockDim.y + threadIdx.y; // chroma row
    int x = bx * 2, y = by * 2;
    if (x >= width || y >= height) return;

    uint8_t* dst_y = dst;
    uint8_t* dst_uv = dst + (size_t)height * dst_pitch_elems;

    float cb_sum = 0.0f, cr_sum = 0.0f;
    int n = 0;
    for (int dy = 0; dy < 2; ++dy) {
        for (int dx = 0; dx < 2; ++dx) {
            int px = x + dx, py = y + dy;
            if (px >= width || py >= height) continue;
            float r10, g10, b10;
            unpack(src[(size_t)py * src_pitch_words + px], r10, g10, b10);
            float r, g, b, yv, cb, cr;
            to_rec709(r10, g10, b10, tonemap, r, g, b);
            ycbcr709(r, g, b, yv, cb, cr);
            dst_y[(size_t)py * dst_pitch_elems + px] = clamp8(yv);
            cb_sum += cb;
            cr_sum += cr;
            ++n;
        }
    }
    float inv = 1.0f / (float)n;
    dst_uv[(size_t)by * dst_pitch_elems + bx * 2 + 0] = clamp8(cb_sum * inv);
    dst_uv[(size_t)by * dst_pitch_elems + bx * 2 + 1] = clamp8(cr_sum * inv);
}

extern "C" __global__ void argb_to_nv12(const uint32_t* src, int src_pitch_words,
                                        uint8_t* dst, int dst_pitch_elems,
                                        int width, int height) {
    convert(src, src_pitch_words, dst, dst_pitch_elems, width, height, false);
}

extern "C" __global__ void argb_to_nv12_tonemap(const uint32_t* src, int src_pitch_words,
                                                uint8_t* dst, int dst_pitch_elems,
                                                int width, int height) {
    convert(src, src_pitch_words, dst, dst_pitch_elems, width, height, true);
}

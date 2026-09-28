// SPDX-License-Identifier: GPL-2.0-or-later

//! scRGB FP16 → P010 (BT.2020 PQ) colour convert, for the D3D11 capture path.
//!
//! DDA/WGC hand back an scRGB linear FP16 texture ([`sunburst_capture::Frame::
//! Texture`]); NVENC wants 10-bit YUV 4:2:0 (P010). This runs the CLAUDE.md
//! convert as a D3D11 **compute** shader: normalise by 80 nits → Rec.709→BT.2020
//! primaries → PQ EOTF⁻¹ → BT.2020 non-constant-luminance YCbCr, 4:2:0
//! subsampled, written 10-bit-left-justified into a P010 texture.
//!
//! The shader is runtime-compiled with `D3DCompile` (no build-time fxc, so
//! `cargo xwin` stays clean) and the device is taken from the input texture
//! (`GetDevice`), so no device has to be threaded through the pipeline.
//!
//! **Box-validation caveat:** the colour maths is exact and version-independent,
//! but the P010 plane UAV binding (Y as `R16_UNORM`, UV as `R16G16_UNORM`) leans
//! on D3D11 plane-view support that varies by driver — it is verified on the 4070,
//! not here (this host has no GPU).

use windows::Win32::Graphics::Direct3D::D3D_SHADER_MACRO;
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_ASYNC_GETDATA_DONOTFLUSH, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_BIND_UNORDERED_ACCESS, D3D11_CPU_ACCESS_FLAG, D3D11_QUERY_DESC, D3D11_QUERY_EVENT,
    D3D11_RESOURCE_MISC_FLAG, D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_UAV, D3D11_TEXTURE2D_DESC,
    D3D11_UAV_DIMENSION_TEXTURE2D, D3D11_UNORDERED_ACCESS_VIEW_DESC,
    D3D11_UNORDERED_ACCESS_VIEW_DESC_0, D3D11_USAGE_DEFAULT, ID3D11ComputeShader, ID3D11Device,
    ID3D11DeviceContext, ID3D11Query, ID3D11ShaderResourceView, ID3D11Texture2D,
    ID3D11UnorderedAccessView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM,
    DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R16G16_UNORM,
};
use windows::core::{PCSTR, s};

/// The scRGB FP16 → P010 (BT.2020 PQ) compute shader. One thread per 2×2 block
/// (one chroma sample); the maths is CLAUDE.md's HDR convert, exactly.
const SHADER_HLSL: &[u8] = br#"
Texture2D<float4>   src   : register(t0);   // scRGB linear FP16 (1.0 == 80 nits)
RWTexture2D<float>  dstY  : register(u0);   // P010 luma plane
RWTexture2D<float2> dstUV : register(u1);   // P010 chroma plane (half res)

// Rec.709 -> Rec.2020 linear primaries.
static const float3x3 REC709_TO_2020 = {
    0.627404, 0.329283, 0.043313,
    0.069097, 0.919540, 0.011362,
    0.016391, 0.088013, 0.895595
};

float pq_encode(float l) {           // l normalised so 1.0 == 10000 nits
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    l = max(l, 0.0);
    float lp = pow(l, m1);
    return pow((c1 + c2 * lp) / (1.0 + c3 * lp), m2);
}

float3 scrgb_to_pq2020(float3 rgb) {
    float3 lin = mul(REC709_TO_2020, max(rgb, 0.0) * (80.0 / 10000.0));
    return float3(pq_encode(lin.r), pq_encode(lin.g), pq_encode(lin.b));
}

// BT.2020 non-constant luminance, limited (studio) range, 10-bit codes.
float3 rgb_to_ycbcr(float3 c) {
    float y  = 0.2627 * c.r + 0.6780 * c.g + 0.0593 * c.b;
    float cb = (c.b - y) / 1.8814;
    float cr = (c.r - y) / 1.4746;
    return float3(64.0 + y * 876.0, 512.0 + cb * 896.0, 512.0 + cr * 896.0);
}

// A 10-bit code, left-justified into 16-bit UNORM (P010): code * 64 / 65535.
float p010(float code10) { return saturate(code10 * 64.0 / 65535.0); }

float3 sample_ycc(uint2 p) { return rgb_to_ycbcr(scrgb_to_pq2020(src[p].rgb)); }

[numthreads(8, 8, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    uint2 p = tid.xy * 2;                 // top-left of this 2x2 block
    float3 a = sample_ycc(p + uint2(0, 0));
    float3 b = sample_ycc(p + uint2(1, 0));
    float3 c = sample_ycc(p + uint2(0, 1));
    float3 d = sample_ycc(p + uint2(1, 1));
    dstY[p + uint2(0, 0)] = p010(a.x);
    dstY[p + uint2(1, 0)] = p010(b.x);
    dstY[p + uint2(0, 1)] = p010(c.x);
    dstY[p + uint2(1, 1)] = p010(d.x);
    float cb = (a.y + b.y + c.y + d.y) * 0.25;
    float cr = (a.z + b.z + c.z + d.z) * 0.25;
    dstUV[tid.xy] = float2(p010(cb), p010(cr));
}
"#;

/// The BT.709 SDR compute shader. Same 2×2 structure as the P010 shader, Rec.709.
/// `TEN_BIT` selects 10-bit P010 output (SDR HEVC/AV1) vs 8-bit NV12 (H.264).
/// `TONEMAP` (an HDR-range source) rolls HDR off to SDR with an ACES curve, else it
/// clamps an already-SDR source. `SRGB_INPUT` decodes an 8-bit gamma desktop (DDA
/// in SDR) to linear; without it the input is scRGB FP16 (already linear).
const SDR_SHADER_HLSL: &[u8] = br#"
Texture2D<float4>   src   : register(t0);   // scRGB linear FP16 (1.0 == 80 nits)
RWTexture2D<float>  dstY  : register(u0);   // NV12 luma plane (R8)
RWTexture2D<float2> dstUV : register(u1);   // NV12 chroma plane (R8G8, half res)

// ACES filmic (Narkowicz): roll HDR-range linear light off into [0, 1].
float3 aces(float3 x) {
    const float a = 2.51, b = 0.03, c = 2.43, d = 0.59, e = 0.14;
    return saturate((x * (a * x + b)) / (x * (c * x + d) + e));
}

// Rec.709 opto-electronic transfer (gamma), the SDR video convention.
float bt709_oetf(float c) {
    c = saturate(c);
    return c < 0.018 ? 4.5 * c : 1.099 * pow(c, 0.45) - 0.099;
}

#ifdef SRGB_INPUT
// sRGB EOTF: an 8-bit gamma-encoded desktop (DDA in SDR) decoded to linear.
float srgb_to_linear1(float c) {
    c = saturate(c);
    return c <= 0.04045 ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4);
}
float3 decode_input(float3 c) {
    return float3(srgb_to_linear1(c.r), srgb_to_linear1(c.g), srgb_to_linear1(c.b));
}
#else
float3 decode_input(float3 c) { return c; }   // scRGB FP16 is already linear
#endif

float3 to_display(float3 rgb) {
    float3 lin = decode_input(max(rgb, 0.0));   // linear, 1.0 == 80 nits (SDR white)
#ifdef TONEMAP
    lin = aces(lin);              // HDR source: compress to SDR range
#else
    lin = saturate(lin);          // SDR source: clamp
#endif
    return float3(bt709_oetf(lin.r), bt709_oetf(lin.g), bt709_oetf(lin.b));
}

// Rec.709 non-constant luminance, limited (studio) range, 8-bit codes.
float3 rgb_to_ycbcr(float3 c) {   // c is gamma-encoded [0, 1]
    float y  = 0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b;
    float cb = (c.b - y) / 1.8556;
    float cr = (c.r - y) / 1.5748;
#ifdef TEN_BIT
    return float3(64.0 + y * 876.0, 512.0 + cb * 896.0, 512.0 + cr * 896.0);
#else
    return float3(16.0 + y * 219.0, 128.0 + cb * 224.0, 128.0 + cr * 224.0);
#endif
}

// A code into its UNORM texel: 10-bit left-justified into R16 (P010) under
// TEN_BIT, else 8-bit into R8 (NV12).
#ifdef TEN_BIT
float pack(float code) { return saturate(code * 64.0 / 65535.0); }
#else
float pack(float code) { return saturate(code / 255.0); }
#endif

float3 sample_ycc(uint2 p) { return rgb_to_ycbcr(to_display(src[p].rgb)); }

[numthreads(8, 8, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    uint2 p = tid.xy * 2;
    float3 a = sample_ycc(p + uint2(0, 0));
    float3 b = sample_ycc(p + uint2(1, 0));
    float3 c = sample_ycc(p + uint2(0, 1));
    float3 d = sample_ycc(p + uint2(1, 1));
    dstY[p + uint2(0, 0)] = pack(a.x);
    dstY[p + uint2(1, 0)] = pack(b.x);
    dstY[p + uint2(0, 1)] = pack(c.x);
    dstY[p + uint2(1, 1)] = pack(d.x);
    float cb = (a.y + b.y + c.y + d.y) * 0.25;
    float cr = (a.z + b.z + c.z + d.z) * 0.25;
    dstUV[tid.xy] = float2(pack(cb), pack(cr));
}
"#;

/// The output pixel format the converter produces: 10-bit P010 (BT.2020 PQ, for
/// HEVC/AV1) or 8-bit NV12 (BT.709 SDR, for H.264).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConvertOutput {
    /// 10-bit P010, BT.2020 PQ — HDR HEVC/AV1.
    P010,
    /// 10-bit P010, BT.709 — SDR HEVC/AV1 (same surface as `P010`, SDR transfer).
    P010Sdr,
    /// 8-bit NV12, BT.709 — H.264 SDR.
    Nv12,
}

impl ConvertOutput {
    /// The output texture format and its (luma, chroma) plane view formats.
    fn formats(self) -> (DXGI_FORMAT, DXGI_FORMAT, DXGI_FORMAT) {
        match self {
            ConvertOutput::P010 | ConvertOutput::P010Sdr => (
                DXGI_FORMAT_P010,
                DXGI_FORMAT_R16_UNORM,
                DXGI_FORMAT_R16G16_UNORM,
            ),
            ConvertOutput::Nv12 => (
                DXGI_FORMAT_NV12,
                DXGI_FORMAT_R8_UNORM,
                DXGI_FORMAT_R8G8_UNORM,
            ),
        }
    }
}

/// A P010/NV12 output texture and its two plane UAVs, sized to a resolution.
struct Target {
    width: u32,
    height: u32,
    texture: ID3D11Texture2D,
    y_uav: ID3D11UnorderedAccessView,
    uv_uav: ID3D11UnorderedAccessView,
}

/// scRGB→P010 converter bound to one D3D11 device.
pub struct Converter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    shader: ID3D11ComputeShader,
    output: ConvertOutput,
    target: Option<Target>,
    /// An event query for [`Converter::wait_idle`], made once up front.
    idle: ID3D11Query,
}

impl Converter {
    /// Build a converter on the device that owns `like` (an input texture),
    /// producing `output` (P010 for HEVC/AV1, NV12 for H.264). `hdr_source` says
    /// whether the capture is HDR-range, which the NV12 (SDR) path tonemaps; it
    /// is ignored by the P010 path. The output texture lives on the same device,
    /// as NVENC requires.
    pub fn new(
        like: &ID3D11Texture2D,
        output: ConvertOutput,
        hdr_source: bool,
        srgb_input: bool,
    ) -> Result<Converter, String> {
        // SAFETY: `like` is a live texture; GetDevice/GetImmediateContext hand
        // back refcounted interfaces `windows` releases.
        let device: ID3D11Device = unsafe { like.GetDevice() }.map_err(err("GetDevice"))?;
        // SAFETY: a live device always yields its immediate context.
        let context =
            unsafe { device.GetImmediateContext() }.map_err(err("GetImmediateContext"))?;

        let (src, tonemap, ten_bit) = match output {
            ConvertOutput::P010 => (SHADER_HLSL, false, false),
            // P010Sdr is SDR content, so never tonemap; 10-bit P010 packing.
            ConvertOutput::P010Sdr => (SDR_SHADER_HLSL, false, true),
            ConvertOutput::Nv12 => (SDR_SHADER_HLSL, hdr_source, false),
        };
        let bytecode = compile(src, tonemap, ten_bit, srgb_input)?;
        let mut shader = None;
        // SAFETY: `bytecode` is valid DXBC from D3DCompile; out-param is written.
        unsafe { device.CreateComputeShader(&bytecode, None, Some(&mut shader)) }
            .map_err(err("CreateComputeShader"))?;
        let shader = shader.ok_or_else(|| "no compute shader".to_string())?;

        let mut idle = None;
        let desc = D3D11_QUERY_DESC {
            Query: D3D11_QUERY_EVENT,
            MiscFlags: 0,
        };
        // SAFETY: a valid event-query description; out-param is written.
        unsafe { device.CreateQuery(&desc, Some(&mut idle)) }.map_err(err("CreateQuery"))?;
        let idle = idle.ok_or_else(|| "no event query".to_string())?;

        Ok(Converter {
            device,
            context,
            shader,
            output,
            target: None,
            idle,
        })
    }

    /// Convert `src` (scRGB FP16) to P010 and return the P010 texture. Valid until
    /// the next `convert`. `src` must belong to this converter's device.
    pub fn convert(
        &mut self,
        src: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Result<&ID3D11Texture2D, String> {
        self.ensure_target(width, height)?;
        let target = self.target.as_ref().expect("target ensured above");

        // An SRV over the whole input texture.
        let mut srv: Option<ID3D11ShaderResourceView> = None;
        // SAFETY: `src` is a live texture on this device; default SRV desc.
        unsafe {
            self.device
                .CreateShaderResourceView(src, None, Some(&mut srv))
        }
        .map_err(err("CreateShaderResourceView"))?;

        // SAFETY: all bound resources are live; dispatch covers ⌈w/2⌉×⌈h/2⌉ blocks
        // at 8×8 threads each.
        unsafe {
            self.context.CSSetShader(&self.shader, None);
            self.context.CSSetShaderResources(0, Some(&[srv.clone()]));
            self.context.CSSetUnorderedAccessViews(
                0,
                2,
                Some([Some(target.y_uav.clone()), Some(target.uv_uav.clone())].as_ptr()),
                None,
            );
            let groups_x = width.div_ceil(16); // 8 threads × 2 px per thread
            let groups_y = height.div_ceil(16);
            self.context.Dispatch(groups_x, groups_y, 1);
            // Unbind the UAVs so the texture can be read by NVENC next, and the
            // SRV so the context stops referencing the capture surface — which
            // the backend recycles at its next acquire.
            self.context
                .CSSetUnorderedAccessViews(0, 2, Some([None, None].as_ptr()), None);
            self.context.CSSetShaderResources(0, Some(&[None]));
        }

        Ok(&self.target.as_ref().unwrap().texture)
    }

    /// Block until every convert submitted so far has finished on the GPU.
    ///
    /// For a frame the capture governor *holds* rather than encodes at once.
    /// An encoded frame needs no wait — NVENC reads the output, so the convert
    /// has finished before the bitstream exists — but a held frame goes straight
    /// back to `acquire`, which releases the capture surface (DDA's
    /// `ReleaseFrame`, WGC's pool slot) while the dispatch reading it may still
    /// be queued. Off the latency path: a held frame is early by definition.
    pub fn wait_idle(&self) -> Result<(), String> {
        // SAFETY: a live query on this context; `End` marks everything issued so
        // far, and `Flush` makes sure it is actually submitted.
        unsafe {
            self.context.End(&self.idle);
            self.context.Flush();
        }
        // Bounded, so a hung GPU surfaces as an error rather than a wedged
        // capture thread.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        loop {
            // An event query's payload is a BOOL (TRUE once complete). windows-rs
            // maps "not ready yet" (S_FALSE) to `Ok(())`, so the payload is the
            // only real signal.
            let mut done: i32 = 0;
            // SAFETY: `done` is a BOOL-sized buffer that outlives the call.
            unsafe {
                self.context.GetData(
                    &self.idle,
                    Some((&mut done as *mut i32).cast()),
                    size_of::<i32>() as u32,
                    D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
                )
            }
            .map_err(err("GetData"))?;
            if done != 0 {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err("convert did not complete within 500 ms".into());
            }
            std::thread::yield_now();
        }
    }

    /// (Re)create the P010 target when the resolution changes.
    fn ensure_target(&mut self, width: u32, height: u32) -> Result<(), String> {
        if let Some(t) = &self.target
            && t.width == width
            && t.height == height
        {
            return Ok(());
        }

        let (tex_format, y_format, uv_format) = self.output.formats();
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: tex_format,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_UNORDERED_ACCESS
                | D3D11_BIND_SHADER_RESOURCE
                | D3D11_BIND_RENDER_TARGET)
                .0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_FLAG(0).0 as u32,
            MiscFlags: D3D11_RESOURCE_MISC_FLAG(0).0 as u32,
        };
        let mut texture: Option<ID3D11Texture2D> = None;
        // SAFETY: `desc` is a valid P010/NV12 texture description; no initial data.
        unsafe {
            self.device.CreateTexture2D(
                &desc,
                None::<*const D3D11_SUBRESOURCE_DATA>,
                Some(&mut texture),
            )
        }
        .map_err(err("CreateTexture2D"))?;
        let texture = texture.ok_or_else(|| "no output texture".to_string())?;

        // Plane UAVs: luma is plane 0, chroma plane 1; the plane is selected by
        // the view format (R16/R16G16 for P010, R8/R8G8 for NV12).
        let y_uav = self.plane_uav(&texture, y_format)?;
        let uv_uav = self.plane_uav(&texture, uv_format)?;

        self.target = Some(Target {
            width,
            height,
            texture,
            y_uav,
            uv_uav,
        });
        Ok(())
    }

    fn plane_uav(
        &self,
        texture: &ID3D11Texture2D,
        format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    ) -> Result<ID3D11UnorderedAccessView, String> {
        let desc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
            Format: format,
            ViewDimension: D3D11_UAV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_UAV { MipSlice: 0 },
            },
        };
        let mut uav: Option<ID3D11UnorderedAccessView> = None;
        // SAFETY: `texture` is a live P010 resource; `desc` selects one plane.
        unsafe {
            self.device
                .CreateUnorderedAccessView(texture, Some(&desc), Some(&mut uav))
        }
        .map_err(err("CreateUnorderedAccessView"))?;
        uav.ok_or_else(|| "no UAV".to_string())
    }
}

/// Runtime-compile `src` to DXBC. `tonemap` defines `TONEMAP` for the SDR shader
/// (HDR→SDR roll-off); it is inert in the P010 shader.
fn compile(src: &[u8], tonemap: bool, ten_bit: bool, srgb_input: bool) -> Result<Vec<u8>, String> {
    let mut code = None;
    let mut errors = None;
    // A `{name, definition}` list terminated by `{null, null}`, per D3DCompile.
    let mut defines: Vec<D3D_SHADER_MACRO> = Vec::new();
    if tonemap {
        defines.push(D3D_SHADER_MACRO {
            Name: s!("TONEMAP"),
            Definition: s!("1"),
        });
    }
    if ten_bit {
        defines.push(D3D_SHADER_MACRO {
            Name: s!("TEN_BIT"),
            Definition: s!("1"),
        });
    }
    if srgb_input {
        defines.push(D3D_SHADER_MACRO {
            Name: s!("SRGB_INPUT"),
            Definition: s!("1"),
        });
    }
    let pdefines = if defines.is_empty() {
        None
    } else {
        defines.push(D3D_SHADER_MACRO {
            Name: PCSTR::null(),
            Definition: PCSTR::null(),
        });
        Some(defines.as_ptr())
    };
    // SAFETY: source is a valid byte slice; entry/target are NUL-terminated; the
    // macro list is NUL-terminated and outlives the call; out-params are written.
    let hr = unsafe {
        D3DCompile(
            src.as_ptr().cast(),
            src.len(),
            PCSTR::null(),
            pdefines,
            None,
            s!("main"),
            s!("cs_5_0"),
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if hr.is_err() {
        let msg = errors
            .as_ref()
            .map(|b| {
                // SAFETY: an error blob is a NUL-terminated ASCII string.
                unsafe {
                    let p = b.GetBufferPointer() as *const u8;
                    let n = b.GetBufferSize();
                    String::from_utf8_lossy(std::slice::from_raw_parts(p, n)).into_owned()
                }
            })
            .unwrap_or_else(|| "unknown error".into());
        return Err(format!("D3DCompile: {msg}"));
    }
    let blob = code.ok_or_else(|| "D3DCompile produced no code".to_string())?;
    // SAFETY: a valid DXBC blob; copy it out before the blob drops.
    let bytes = unsafe {
        let p = blob.GetBufferPointer() as *const u8;
        let n = blob.GetBufferSize();
        std::slice::from_raw_parts(p, n).to_vec()
    };
    Ok(bytes)
}

/// A `windows` error → a `String` context.
fn err(what: &'static str) -> impl Fn(windows::core::Error) -> String {
    move |e| format!("{what}: {e}")
}

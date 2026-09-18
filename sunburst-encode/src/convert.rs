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

use windows::Win32::Graphics::Direct3D::Fxc::{D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_UNORDERED_ACCESS,
    D3D11_CPU_ACCESS_FLAG, D3D11_RESOURCE_MISC_FLAG, D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_UAV,
    D3D11_TEXTURE2D_DESC, D3D11_UAV_DIMENSION_TEXTURE2D, D3D11_UNORDERED_ACCESS_VIEW_DESC,
    D3D11_UNORDERED_ACCESS_VIEW_DESC_0, D3D11_USAGE_DEFAULT, ID3D11ComputeShader, ID3D11Device,
    ID3D11DeviceContext, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11UnorderedAccessView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_P010, DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R16G16_UNORM,
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

/// A P010 output texture and its two plane UAVs, sized to a resolution.
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
    target: Option<Target>,
}

impl Converter {
    /// Build a converter on the device that owns `like` (an input texture). The
    /// converted P010 texture lives on the same device, as NVENC requires.
    pub fn new(like: &ID3D11Texture2D) -> Result<Converter, String> {
        // SAFETY: `like` is a live texture; GetDevice/GetImmediateContext hand
        // back refcounted interfaces `windows` releases.
        let device: ID3D11Device = unsafe { like.GetDevice() }.map_err(err("GetDevice"))?;
        // SAFETY: a live device always yields its immediate context.
        let context =
            unsafe { device.GetImmediateContext() }.map_err(err("GetImmediateContext"))?;

        let bytecode = compile()?;
        let mut shader = None;
        // SAFETY: `bytecode` is valid DXBC from D3DCompile; out-param is written.
        unsafe { device.CreateComputeShader(&bytecode, None, Some(&mut shader)) }
            .map_err(err("CreateComputeShader"))?;
        let shader = shader.ok_or_else(|| "no compute shader".to_string())?;

        Ok(Converter {
            device,
            context,
            shader,
            target: None,
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
            // Unbind the UAVs so the texture can be read by NVENC next.
            self.context
                .CSSetUnorderedAccessViews(0, 2, Some([None, None].as_ptr()), None);
        }

        Ok(&self.target.as_ref().unwrap().texture)
    }

    /// (Re)create the P010 target when the resolution changes.
    fn ensure_target(&mut self, width: u32, height: u32) -> Result<(), String> {
        if let Some(t) = &self.target
            && t.width == width
            && t.height == height
        {
            return Ok(());
        }

        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_P010,
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
        // SAFETY: `desc` is a valid P010 texture description; no initial data.
        unsafe {
            self.device.CreateTexture2D(
                &desc,
                None::<*const D3D11_SUBRESOURCE_DATA>,
                Some(&mut texture),
            )
        }
        .map_err(err("CreateTexture2D(P010)"))?;
        let texture = texture.ok_or_else(|| "no P010 texture".to_string())?;

        // Plane UAVs: R16 for luma (plane 0), R16G16 for chroma (plane 1). On
        // D3D11 the plane is selected by the view format on a P010 resource.
        let y_uav = self.plane_uav(&texture, DXGI_FORMAT_R16_UNORM)?;
        let uv_uav = self.plane_uav(&texture, DXGI_FORMAT_R16G16_UNORM)?;

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

/// Runtime-compile [`SHADER_HLSL`] to DXBC.
fn compile() -> Result<Vec<u8>, String> {
    let mut code = None;
    let mut errors = None;
    // SAFETY: source is a valid byte slice; entry/target are NUL-terminated; the
    // out-params are written on success/failure.
    let hr = unsafe {
        D3DCompile(
            SHADER_HLSL.as_ptr().cast(),
            SHADER_HLSL.len(),
            PCSTR::null(),
            None,
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

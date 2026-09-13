// SPDX-License-Identifier: GPL-2.0-or-later

//! Reading a few bytes out of a captured D3D11 texture.
//!
//! Shared by the DDA and WGC backends, which differ in how they *obtain* a
//! texture and not at all in how the signal is read out of it.
//!
//! # Change, not colour
//!
//! The three capture paths hand back three different pixel formats — DDA
//! typically `B8G8R8A8`, WGC configured `R16G16B16A16Float` for HDR, NvFBC
//! `A2B10G10R10`. Decoding a colour would need a branch per format and would be
//! one more thing to get wrong. Comparing **raw bytes against the previous
//! sample** works across all of them, and detecting that the signal *changed* is
//! all the measurement needs: the presenter publishes when it flipped, so the
//! change is what carries the timestamp.
//!
//! # Same device, necessarily
//!
//! The staging texture has to live on the device that owns the captured texture,
//! so this is constructed from whichever device the backend used — DDA's
//! duplication device, or the one handed to WGC's frame pool. It is not a
//! standalone helper with a device of its own.

use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};

/// How much of the read point to pull back. Four pixels square is enough to be
/// robust against a stray single-pixel artefact while staying trivially cheap.
const PATCH: u32 = 4;

/// Bytes compared per sample. Sixteen bytes covers 4×4 at one byte per pixel and
/// a single row at four, which is plenty to see a black/white flip in any format.
pub const SAMPLE_BYTES: usize = 16;

pub struct ReadPoint {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// Recreated if a later frame arrives in a different format, which happens
    /// when the desktop's colour mode changes under a running session.
    staging: Option<(ID3D11Texture2D, DXGI_FORMAT)>,
}

impl ReadPoint {
    pub fn new(device: ID3D11Device, context: ID3D11DeviceContext) -> ReadPoint {
        ReadPoint {
            device,
            context,
            staging: None,
        }
    }

    fn staging_for(&mut self, format: DXGI_FORMAT) -> Result<ID3D11Texture2D, String> {
        if let Some((texture, have)) = &self.staging
            && *have == format
        {
            return Ok(texture.clone());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: PATCH,
            Height: PATCH,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut texture: Option<ID3D11Texture2D> = None;
        // SAFETY: `desc` is fully initialised and `texture` is a valid out
        // parameter.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .map_err(|e| format!("staging CreateTexture2D: {e}"))?;
        let texture = texture.ok_or("CreateTexture2D returned no texture")?;
        self.staging = Some((texture.clone(), format));
        Ok(texture)
    }

    /// Copy `SAMPLE_BYTES` from around (`x`, `y`) of `src` into `out`.
    pub fn sample(
        &mut self,
        src: &ID3D11Texture2D,
        x: u32,
        y: u32,
        out: &mut [u8; SAMPLE_BYTES],
    ) -> Result<(), String> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `desc` is a valid out parameter; GetDesc cannot fail.
        unsafe { src.GetDesc(&mut desc) };
        if x + PATCH > desc.Width || y + PATCH > desc.Height {
            return Err(format!(
                "read point {x},{y} outside the {}x{} capture",
                desc.Width, desc.Height
            ));
        }
        let staging = self.staging_for(desc.Format)?;

        let region = D3D11_BOX {
            left: x,
            top: y,
            front: 0,
            right: x + PATCH,
            bottom: y + PATCH,
            back: 1,
        };
        // SAFETY: both textures are live and share a device, the formats match
        // by construction above, and `region` is inside `src`.
        unsafe {
            self.context
                .CopySubresourceRegion(&staging, 0, 0, 0, 0, src, 0, Some(&region));
        }

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: `staging` is CPU-readable and `mapped` is a valid out pointer.
        unsafe {
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }
        .map_err(|e| format!("Map staging: {e}"))?;

        // SAFETY: Map succeeded, so pData points at at least RowPitch bytes; the
        // copy is bounded by the smaller of that and the output buffer.
        unsafe {
            let available = (mapped.RowPitch as usize).min(SAMPLE_BYTES);
            std::ptr::copy_nonoverlapping(mapped.pData.cast::<u8>(), out.as_mut_ptr(), available);
            self.context.Unmap(&staging, 0);
        }
        Ok(())
    }
}

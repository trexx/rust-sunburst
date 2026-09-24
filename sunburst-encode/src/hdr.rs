// SPDX-License-Identifier: GPL-2.0-or-later

//! HDR mastering metadata — the NVENC `MASTERING_DISPLAY_INFO` +
//! `CONTENT_LIGHT_LEVEL` structs, built from the display's HDR info.
//!
//! The encoder points its per-frame codec pic-params at these (`pMasteringDisplay`
//! / `pMaxCll`), and with `outputMasteringDisplay` / `outputMaxCll` set in the
//! codec config the driver writes the SMPTE ST 2086 mastering-display and content-
//! light-level SEI (HEVC) / metadata OBU (AV1).
//!
//! The scaling to ST 2086 fixed point is not here: it is
//! `sunburst_core::proto::HdrMastering::from_display`, shared with the
//! handshake so the SEI and what the client is told cannot disagree. This is
//! only NVENC's field order.

use sunburst_core::proto::HdrMastering;

/// CIE xy chromaticity in ST 2086 fixed point (increments of 0.00002).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct ChromaPoint {
    pub x: u16,
    pub y: u16,
}

/// `MASTERING_DISPLAY_INFO` — the header orders the primaries **g, b, r**.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct MasteringDisplayInfo {
    pub g: ChromaPoint,
    pub b: ChromaPoint,
    pub r: ChromaPoint,
    pub white_point: ChromaPoint,
    /// Max display-mastering luminance, in 0.0001 cd/m² units.
    pub max_luma: u32,
    /// Min display-mastering luminance, in 0.0001 cd/m² units.
    pub min_luma: u32,
}

/// `CONTENT_LIGHT_LEVEL`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct ContentLightLevel {
    pub max_content_light_level: u16,
    pub max_pic_average_light_level: u16,
}

impl ChromaPoint {
    fn from_xy(xy: [u16; 2]) -> ChromaPoint {
        ChromaPoint { x: xy[0], y: xy[1] }
    }
}

impl MasteringDisplayInfo {
    /// A pure reorder: the scaling to ST 2086 already happened, once, in
    /// `HdrMastering::from_display`, which the handshake shares. NVENC orders
    /// the primaries g, b, r; the wire and the display desc order them r, g, b.
    pub(crate) fn from_mastering(m: &HdrMastering) -> MasteringDisplayInfo {
        let [r, g, b] = m.primaries;
        MasteringDisplayInfo {
            g: ChromaPoint::from_xy(g),
            b: ChromaPoint::from_xy(b),
            r: ChromaPoint::from_xy(r),
            white_point: ChromaPoint::from_xy(m.white),
            max_luma: m.max_luminance,
            min_luma: m.min_luminance,
        }
    }
}

impl ContentLightLevel {
    pub(crate) fn from_mastering(m: &HdrMastering) -> ContentLightLevel {
        ContentLightLevel {
            max_content_light_level: m.max_cll,
            max_pic_average_light_level: m.max_fall,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reorders_rgb_into_nvenc_gbr() {
        let m = HdrMastering {
            primaries: [[1, 2], [3, 4], [5, 6]],
            white: [7, 8],
            max_luminance: 9,
            min_luminance: 10,
            max_cll: 11,
            max_fall: 12,
        };
        let mdi = MasteringDisplayInfo::from_mastering(&m);
        assert_eq!((mdi.r.x, mdi.r.y), (1, 2));
        assert_eq!((mdi.g.x, mdi.g.y), (3, 4));
        assert_eq!((mdi.b.x, mdi.b.y), (5, 6));
        assert_eq!((mdi.white_point.x, mdi.white_point.y), (7, 8));
        assert_eq!((mdi.max_luma, mdi.min_luma), (9, 10));
        let cll = ContentLightLevel::from_mastering(&m);
        assert_eq!(
            (cll.max_content_light_level, cll.max_pic_average_light_level),
            (11, 12)
        );
    }
}

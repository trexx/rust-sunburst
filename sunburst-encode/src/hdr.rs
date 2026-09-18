// SPDX-License-Identifier: GPL-2.0-or-later

//! HDR mastering metadata — the NVENC `MASTERING_DISPLAY_INFO` +
//! `CONTENT_LIGHT_LEVEL` structs, built from the display's HDR info.
//!
//! The encoder points its per-frame codec pic-params at these (`pMasteringDisplay`
//! / `pMaxCll`), and with `outputMasteringDisplay` / `outputMaxCll` set in the
//! codec config the driver writes the SMPTE ST 2086 mastering-display and content-
//! light-level SEI (HEVC) / metadata OBU (AV1).
//!
//! The scaling to ST 2086 fixed point is pure logic and unit-tested.

use sunburst_capture::HdrMetadata;

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

/// CIE xy → ST 2086 fixed point (0.00002 increments → ×50000).
fn chroma(xy: [f32; 2]) -> ChromaPoint {
    ChromaPoint {
        x: (xy[0] * 50_000.0).round().clamp(0.0, 65_535.0) as u16,
        y: (xy[1] * 50_000.0).round().clamp(0.0, 65_535.0) as u16,
    }
}

impl MasteringDisplayInfo {
    pub(crate) fn from_metadata(m: &HdrMetadata) -> MasteringDisplayInfo {
        MasteringDisplayInfo {
            g: chroma(m.green),
            b: chroma(m.blue),
            r: chroma(m.red),
            white_point: chroma(m.white),
            // nits → 0.0001 cd/m² units (×10000).
            max_luma: (m.max_luminance * 10_000.0).round().max(0.0) as u32,
            min_luma: (m.min_luminance * 10_000.0).round().max(0.0) as u32,
        }
    }
}

impl ContentLightLevel {
    pub(crate) fn from_metadata(m: &HdrMetadata) -> ContentLightLevel {
        ContentLightLevel {
            max_content_light_level: m.max_luminance.round().clamp(0.0, 65_535.0) as u16,
            max_pic_average_light_level: m.max_full_frame_luminance.round().clamp(0.0, 65_535.0)
                as u16,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_bt2020_primaries_and_luminance() {
        // A 1000-nit BT.2020 mastering display.
        let m = HdrMetadata {
            red: [0.708, 0.292],
            green: [0.170, 0.797],
            blue: [0.131, 0.046],
            white: [0.3127, 0.3290],
            min_luminance: 0.005,
            max_luminance: 1000.0,
            max_full_frame_luminance: 400.0,
        };
        let mdi = MasteringDisplayInfo::from_metadata(&m);
        assert_eq!(mdi.r.x, 35_400); // 0.708 × 50000
        assert_eq!(mdi.g.y, 39_850); // 0.797 × 50000
        assert_eq!(mdi.b.x, 6_550); //  0.131 × 50000
        assert_eq!(mdi.white_point.x, 15_635); // 0.3127 × 50000
        assert_eq!(mdi.max_luma, 10_000_000); // 1000 × 10000
        assert_eq!(mdi.min_luma, 50); // 0.005 × 10000

        let cll = ContentLightLevel::from_metadata(&m);
        assert_eq!(cll.max_content_light_level, 1000);
        assert_eq!(cll.max_pic_average_light_level, 400);
    }

    #[test]
    fn clamps_absurd_values() {
        let m = HdrMetadata {
            red: [2.0, 2.0],
            green: [0.0, 0.0],
            blue: [0.0, 0.0],
            white: [0.0, 0.0],
            min_luminance: 0.0,
            max_luminance: 100_000.0,
            max_full_frame_luminance: 100_000.0,
        };
        let cll = ContentLightLevel::from_metadata(&m);
        assert_eq!(cll.max_content_light_level, 65_535);
        assert_eq!(MasteringDisplayInfo::from_metadata(&m).r.x, 65_535);
    }
}

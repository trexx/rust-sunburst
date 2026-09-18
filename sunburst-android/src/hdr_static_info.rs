// SPDX-License-Identifier: GPL-2.0-or-later

//! Assemble `MediaFormat.KEY_HDR_STATIC_INFO` — pure, host-tested.
//!
//! Android wants the CTA-861.3 "static metadata type 1" blob: a type byte, the
//! ST 2086 mastering-display primaries and white point, the display luminance
//! range, then MaxCLL/MaxFALL. Getting the units or order wrong is the "HDR is
//! subtly off / text fringes" trap, so the layout is pinned by a test.
//!
//! [`HdrMastering`] already carries chromaticity in 0.00002 units (matching the
//! blob) and luminance in 0.0001 cd/m²; the max-luminance field of the blob is in
//! whole cd/m², so it is scaled down here.

use sunburst_core::proto::HdrMastering;

/// The 25-byte HDR static info for `m`.
pub fn hdr_static_info(m: &HdrMastering) -> [u8; 25] {
    let mut b = [0u8; 25];
    b[0] = 0; // descriptor id: static metadata type 1
    let mut put = |off: usize, v: u16| b[off..off + 2].copy_from_slice(&v.to_le_bytes());
    // Primaries R,G,B then white, x then y — same 0.00002 units as the blob.
    put(1, m.primaries[0][0]);
    put(3, m.primaries[0][1]);
    put(5, m.primaries[1][0]);
    put(7, m.primaries[1][1]);
    put(9, m.primaries[2][0]);
    put(11, m.primaries[2][1]);
    put(13, m.white[0]);
    put(15, m.white[1]);
    // Max display luminance in whole cd/m² (our field is 0.0001 cd/m²); min in
    // 0.0001 cd/m² directly.
    put(17, (m.max_luminance / 10_000).min(u16::MAX as u32) as u16);
    put(19, m.min_luminance.min(u16::MAX as u32) as u16);
    put(21, m.max_cll);
    put(23, m.max_fall);
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_cta_861_3() {
        // BT.2020 primaries and a D65 white in 0.00002 units, 1000 cd/m² peak.
        let m = HdrMastering {
            primaries: [[35_400, 14_600], [8_500, 39_850], [6_550, 2_300]],
            white: [15_635, 16_450],
            max_luminance: 10_000_000, // 1000 cd/m² in 0.0001 units
            min_luminance: 50,         // 0.005 cd/m²
            max_cll: 1000,
            max_fall: 400,
        };
        let b = hdr_static_info(&m);
        assert_eq!(b[0], 0);
        assert_eq!(u16::from_le_bytes([b[1], b[2]]), 35_400); // R.x
        assert_eq!(u16::from_le_bytes([b[15], b[16]]), 16_450); // W.y
        assert_eq!(u16::from_le_bytes([b[17], b[18]]), 1000); // max luminance → cd/m²
        assert_eq!(u16::from_le_bytes([b[19], b[20]]), 50); // min luminance
        assert_eq!(u16::from_le_bytes([b[21], b[22]]), 1000); // MaxCLL
        assert_eq!(u16::from_le_bytes([b[23], b[24]]), 400); // MaxFALL
    }

    #[test]
    fn luminance_saturates_rather_than_wraps() {
        let m = HdrMastering {
            max_luminance: u32::MAX, // /10000 still exceeds u16
            min_luminance: u32::MAX,
            ..Default::default()
        };
        let b = hdr_static_info(&m);
        assert_eq!(u16::from_le_bytes([b[17], b[18]]), u16::MAX);
        assert_eq!(u16::from_le_bytes([b[19], b[20]]), u16::MAX);
    }
}

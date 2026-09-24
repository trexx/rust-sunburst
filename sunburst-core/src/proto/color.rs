// SPDX-License-Identifier: GPL-2.0-or-later

//! What a stream's pixels mean: colour description and HDR mastering.
//!
//! Two things live here because both ends need them and they must not drift:
//!
//! - [`HdrMastering`], and the **one** conversion from a display's reported
//!   primaries and luminance (floats, nits) to the SMPTE ST 2086 fixed point the
//!   bitstream and the handshake both carry. The encoder's SEI and the
//!   `SessionConfig`/`CodecPrivate` blocks are built from the same function, so
//!   what the client is told and what the bitstream says are the same numbers.
//! - [`ColorInfo`], the CICP triple (ISO/IEC 23091-2) plus range and depth an
//!   encoder build signals in its VUI / `color_config`, carried on the wire so a
//!   client can configure its decoder without parsing the bitstream.

use super::control::StreamCodec;

/// HDR mastering metadata, in the SMPTE ST 2086 fixed-point units the
/// bitstream itself carries: chromaticity in 0.00002 steps, luminance in
/// 0.0001 cd/m². The client hands these to `MediaFormat.KEY_HDR_STATIC_INFO`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct HdrMastering {
    /// Red, green, blue `[x, y]`.
    pub primaries: [[u16; 2]; 3],
    pub white: [u16; 2],
    pub max_luminance: u32,
    pub min_luminance: u32,
    pub max_cll: u16,
    pub max_fall: u16,
}

/// CIE xy → ST 2086 fixed point (0.00002 increments, so ×50000).
fn chroma(xy: [f32; 2]) -> [u16; 2] {
    let fixed = |v: f32| (v * 50_000.0).round().clamp(0.0, 65_535.0) as u16;
    [fixed(xy[0]), fixed(xy[1])]
}

/// Nits → ST 2086 luminance (0.0001 cd/m² increments, so ×10000).
fn luminance(nits: f32) -> u32 {
    (nits * 10_000.0).round().clamp(0.0, u32::MAX as f32) as u32
}

/// Nits → a whole-nit content light level.
fn light_level(nits: f32) -> u16 {
    nits.round().clamp(0.0, 65_535.0) as u16
}

impl HdrMastering {
    /// From a display's own description: CIE xy primaries and white point, and
    /// luminance in nits, as DXGI's `GetDesc1` reports them.
    ///
    /// MaxCLL and MaxFALL are the **display's** limits (`max_nits`,
    /// `max_fall_nits`), not measured from content: the desktop can light any
    /// pixel the panel can, and a measured MaxCLL would need a pass over every
    /// frame. That is the first cut, and it errs towards a decoder tone-mapping
    /// less, not more.
    pub fn from_display(
        red: [f32; 2],
        green: [f32; 2],
        blue: [f32; 2],
        white: [f32; 2],
        min_nits: f32,
        max_nits: f32,
        max_fall_nits: f32,
    ) -> HdrMastering {
        HdrMastering {
            primaries: [chroma(red), chroma(green), chroma(blue)],
            white: chroma(white),
            max_luminance: luminance(max_nits),
            min_luminance: luminance(min_nits),
            max_cll: light_level(max_nits),
            max_fall: light_level(max_fall_nits),
        }
    }
}

/// CICP code points (ISO/IEC 23091-2, the values H.273, HEVC's VUI and AV1's
/// `color_config` all share). Only the ones this server emits.
pub mod cicp {
    pub const PRIMARIES_BT709: u8 = 1;
    pub const PRIMARIES_BT2020: u8 = 9;
    pub const TRANSFER_BT709: u8 = 1;
    /// SMPTE ST 2084, "PQ".
    pub const TRANSFER_PQ: u8 = 16;
    pub const MATRIX_BT709: u8 = 1;
    /// BT.2020 non-constant luminance.
    pub const MATRIX_BT2020_NCL: u8 = 9;
}

/// The colour description one encoder build signals.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ColorInfo {
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    /// Full (PC) range rather than limited (studio). The server encodes
    /// limited throughout; the field exists so a client never has to assume.
    pub full_range: bool,
    /// Luma bit depth: 8 or 10.
    pub bit_depth: u8,
}

impl ColorInfo {
    /// H.264: 8-bit BT.709 SDR, the only thing it carries here.
    pub const SDR_709_8: ColorInfo = ColorInfo {
        primaries: cicp::PRIMARIES_BT709,
        transfer: cicp::TRANSFER_BT709,
        matrix: cicp::MATRIX_BT709,
        full_range: false,
        bit_depth: 8,
    };
    /// HEVC/AV1 from an SDR desktop: 10-bit BT.709.
    pub const SDR_709_10: ColorInfo = ColorInfo {
        bit_depth: 10,
        ..ColorInfo::SDR_709_8
    };
    /// HEVC/AV1 from an HDR desktop: BT.2020 PQ, 10-bit.
    pub const HDR10: ColorInfo = ColorInfo {
        primaries: cicp::PRIMARIES_BT2020,
        transfer: cicp::TRANSFER_PQ,
        matrix: cicp::MATRIX_BT2020_NCL,
        full_range: false,
        bit_depth: 10,
    };

    /// Whether this is a PQ (HDR) signal.
    pub fn is_pq(&self) -> bool {
        self.transfer == cicp::TRANSFER_PQ
    }

    /// Whether this description, with this mastering, is one the server can
    /// legitimately send for `codec`. For receivers that want to catch a
    /// server bug rather than configure a decoder around it (fakeclient).
    pub fn check(
        &self,
        codec: StreamCodec,
        hdr: Option<&HdrMastering>,
    ) -> Result<(), &'static str> {
        if !matches!(self.bit_depth, 8 | 10) {
            return Err("bit depth is neither 8 nor 10");
        }
        if codec == StreamCodec::H264 && *self != ColorInfo::SDR_709_8 {
            return Err("H.264 here is 8-bit BT.709 SDR only");
        }
        if codec != StreamCodec::H264 && self.bit_depth != 10 {
            return Err("HEVC/AV1 are 10-bit here");
        }
        if self.is_pq() {
            if self.primaries != cicp::PRIMARIES_BT2020 || self.matrix != cicp::MATRIX_BT2020_NCL {
                return Err("PQ without BT.2020 primaries and matrix");
            }
            if let Some(m) = hdr
                && m.max_luminance <= m.min_luminance
            {
                return Err("mastering max luminance is not above min");
            }
        } else if hdr.is_some() {
            return Err("mastering metadata on a non-PQ stream");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1000-nit BT.2020 mastering display: the vector every HDR test in the
    /// tree shares, so the handshake, the SEI and the client blob agree.
    fn bt2020_1000() -> HdrMastering {
        HdrMastering::from_display(
            [0.708, 0.292],
            [0.170, 0.797],
            [0.131, 0.046],
            [0.3127, 0.3290],
            0.005,
            1000.0,
            400.0,
        )
    }

    #[test]
    fn scales_bt2020_primaries_and_luminance() {
        let m = bt2020_1000();
        assert_eq!(m.primaries[0], [35_400, 14_600]); // red, ×50000
        assert_eq!(m.primaries[1], [8_500, 39_850]); // green
        assert_eq!(m.primaries[2], [6_550, 2_300]); // blue
        assert_eq!(m.white, [15_635, 16_450]);
        assert_eq!(m.max_luminance, 10_000_000); // 1000 nits, ×10000
        assert_eq!(m.min_luminance, 50); // 0.005 nits
        assert_eq!(m.max_cll, 1000);
        assert_eq!(m.max_fall, 400);
    }

    #[test]
    fn clamps_absurd_values() {
        let m = HdrMastering::from_display(
            [2.0, -1.0],
            [0.0; 2],
            [0.0; 2],
            [0.0; 2],
            -5.0,
            100_000.0,
            100_000.0,
        );
        assert_eq!(m.primaries[0], [65_535, 0]);
        assert_eq!(m.min_luminance, 0);
        assert_eq!(m.max_cll, 65_535);
        assert_eq!(m.max_fall, 65_535);
        assert_eq!(m.max_luminance, 1_000_000_000);
    }

    #[test]
    fn what_the_server_sends_passes_its_own_check() {
        let m = bt2020_1000();
        assert!(ColorInfo::SDR_709_8.check(StreamCodec::H264, None).is_ok());
        for codec in [StreamCodec::Hevc, StreamCodec::Av1] {
            assert!(ColorInfo::SDR_709_10.check(codec, None).is_ok());
            assert!(ColorInfo::HDR10.check(codec, Some(&m)).is_ok());
            // PQ without mastering is legal: NvFBC's path may not have it.
            assert!(ColorInfo::HDR10.check(codec, None).is_ok());
        }
    }

    #[test]
    fn contradictions_are_caught() {
        let m = bt2020_1000();
        let cases: [(ColorInfo, StreamCodec, Option<&HdrMastering>); 6] = [
            (ColorInfo::HDR10, StreamCodec::H264, Some(&m)),
            (ColorInfo::SDR_709_10, StreamCodec::H264, None),
            (ColorInfo::SDR_709_8, StreamCodec::Hevc, None),
            (ColorInfo::SDR_709_10, StreamCodec::Av1, Some(&m)),
            (
                ColorInfo {
                    primaries: cicp::PRIMARIES_BT709,
                    ..ColorInfo::HDR10
                },
                StreamCodec::Hevc,
                None,
            ),
            (
                ColorInfo::HDR10,
                StreamCodec::Av1,
                Some(&HdrMastering {
                    max_luminance: 10,
                    min_luminance: 10,
                    ..m
                }),
            ),
        ];
        for (i, (color, codec, hdr)) in cases.into_iter().enumerate() {
            assert!(color.check(codec, hdr).is_err(), "case {i} passed");
        }
    }
}

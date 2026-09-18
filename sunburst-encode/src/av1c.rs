// SPDX-License-Identifier: GPL-2.0-or-later

//! The AV1 codec-configuration record (`av1C`) — the client's MediaCodec `csd-0`.
//!
//! `av1C` is 4 header bytes then the `configOBUs` (the sequence-header OBU the
//! encoder produced, fetched via `Encoder::sequence_header`). A wrong `av1C`
//! configures the decoder fine and then silently outputs nothing (CLAUDE.md), so
//! the byte assembly here is unit-tested; the profile/level/tier are what the
//! encoder was configured for (profile 0, 10-bit 4:2:0). Parsing them back out of
//! the OBU for exact agreement is a later refinement.

/// Assemble an `AV1CodecConfigurationRecord` from its fields and the sequence OBU.
///
/// - `seq_profile` — 3 bits (0 = Main).
/// - `seq_level_idx` — 5 bits (`seq_level_idx_0`); see [`seq_level_idx`].
/// - `seq_tier` — 1 bit (0 = Main tier).
/// - `high_bitdepth` — true for 10-bit.
///
/// Chroma is fixed at 4:2:0 (`subsampling_x = subsampling_y = 1`), which is what
/// the P010 pipeline produces.
pub fn av1c_record(
    seq_profile: u8,
    seq_level_idx: u8,
    seq_tier: u8,
    high_bitdepth: bool,
    seq_header_obu: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + seq_header_obu.len());
    // marker(1)=1, version(7)=1.
    out.push(0x81);
    // seq_profile(3) | seq_level_idx_0(5).
    out.push((seq_profile << 5) | (seq_level_idx & 0x1F));
    // seq_tier_0(1) | high_bitdepth(1) | twelve_bit(1)=0 | monochrome(1)=0 |
    // chroma_subsampling_x(1)=1 | chroma_subsampling_y(1)=1 | chroma_sample_position(2)=0.
    out.push((seq_tier << 7) | (u8::from(high_bitdepth) << 6) | (1 << 3) | (1 << 2));
    // reserved(3)=0 | initial_presentation_delay_present(1)=0 | reserved(4)=0.
    out.push(0);
    out.extend_from_slice(seq_header_obu);
    out
}

/// The `seq_level_idx_0` for a resolution, by the AV1 level table (assuming ≤60fps
/// 4:2:0) — 1080p → 4.0 (8), 4K → 5.1 (13), above → 6.0 (16).
pub fn seq_level_idx(width: u32, height: u32) -> u8 {
    let pixels = u64::from(width) * u64::from(height);
    if pixels <= 1920 * 1080 {
        8
    } else if pixels <= 3840 * 2160 {
        13
    } else {
        16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_header_bytes_for_main_10bit_420() {
        // Profile 0, level 5.1 (13), Main tier, 10-bit.
        let obu = [0xAA, 0xBB, 0xCC];
        let rec = av1c_record(0, 13, 0, true, &obu);
        assert_eq!(
            rec,
            vec![
                0x81, // marker + version
                0x0D, // (0<<5) | 13
                0x4C, // high_bitdepth(1<<6) | subsampling_x(1<<3) | subsampling_y(1<<2)
                0x00, // reserved
                0xAA, 0xBB, 0xCC, // the sequence OBU
            ]
        );
    }

    #[test]
    fn tier_and_8bit_flip_the_expected_bits() {
        let rec = av1c_record(0, 8, 1, false, &[]);
        // seq_tier(1<<7) | subsampling bits(0x0C); high_bitdepth clear.
        assert_eq!(rec, vec![0x81, 0x08, 0x8C, 0x00]);
    }

    #[test]
    fn levels_by_resolution() {
        assert_eq!(seq_level_idx(1920, 1080), 8);
        assert_eq!(seq_level_idx(3840, 2160), 13);
        assert_eq!(seq_level_idx(7680, 4320), 16);
    }
}

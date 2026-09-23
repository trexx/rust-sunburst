// SPDX-License-Identifier: GPL-2.0-or-later

//! AV1 bitstream facts the server needs, as pure functions over bytes.
//!
//! Three things, all written from the AV1 specification and host-tested here
//! rather than in the Windows-only encoder crate:
//!
//! - **OBU walking** for the subframe drain. NVENC's AV1 subframe write keeps
//!   growing `bitstreamSizeInBytes` past the end of the frame when polled, so
//!   the drain bounds its read from the bitstream itself: a frame is complete
//!   once a tile-group OBU reports the last tile (`tg_end == NumTiles − 1`).
//! - **The tile grid.** Uniform tile spacing does not always give `2^log2`
//!   tiles: at 4K (34 superblock rows) asking for 8 rows yields 7. The drain has
//!   to know the real count, and the encoder has to ask for a grid the level
//!   allows.
//! - **The sequence header** — profile, level, tier and superblock size — so the
//!   `av1C` record agrees with the OBU it wraps, and the grid uses the
//!   superblock size the encoder actually chose.

/// `OBU_SEQUENCE_HEADER`.
pub const OBU_SEQUENCE_HEADER: u8 = 1;
/// `OBU_TILE_GROUP`.
pub const OBU_TILE_GROUP: u8 = 4;
/// `OBU_FRAME`: a frame header and a tile group in one OBU.
pub const OBU_FRAME: u8 = 6;

/// Read an AV1 `leb128()` from `buf[pos..end]`, returning the value and the
/// position just past it, or `None` if it is not fully present within `end`
/// (its bytes have not arrived yet) or runs past eight bytes.
pub fn read_leb128(buf: &[u8], mut pos: usize, end: usize) -> Option<(u64, usize)> {
    let end = end.min(buf.len());
    let mut value: u64 = 0;
    for i in 0..8u32 {
        if pos >= end {
            return None;
        }
        let b = buf[pos];
        pos += 1;
        value |= u64::from(b & 0x7f) << (i * 7);
        if b & 0x80 == 0 {
            return Some((value, pos));
        }
    }
    None
}

/// One OBU's header, as located in a byte buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Obu {
    pub obu_type: u8,
    /// Where the payload starts.
    pub payload_start: usize,
    /// One past the payload's last byte.
    pub end: usize,
    /// Whether the OBU carried `obu_size`. Without it the OBU runs to the end of
    /// the buffer, which a caller reading a still-growing buffer cannot trust.
    pub has_size: bool,
}

/// Decode the OBU header at `buf[pos]`, using only the bytes before `end`.
/// `None` if the header, its size field, or its whole payload has not arrived.
pub fn obu_header(buf: &[u8], pos: usize, end: usize) -> Option<Obu> {
    let end = end.min(buf.len());
    if pos >= end {
        return None;
    }
    let b0 = buf[pos];
    // forbidden(1) type(4) extension(1) has_size(1) reserved(1).
    let obu_type = (b0 >> 3) & 0x0f;
    let ext = (b0 >> 2) & 1 == 1;
    let has_size = (b0 >> 1) & 1 == 1;
    let mut p = pos + 1;
    if ext {
        if p >= end {
            return None;
        }
        p += 1;
    }
    if !has_size {
        return Some(Obu {
            obu_type,
            payload_start: p,
            end,
            has_size: false,
        });
    }
    let (size, after) = read_leb128(buf, p, end)?;
    let obu_end = after.checked_add(usize::try_from(size).ok()?)?;
    if obu_end > end {
        return None;
    }
    Some(Obu {
        obu_type,
        payload_start: after,
        end: obu_end,
        has_size: true,
    })
}

/// MSB-first bit reader over a byte slice.
struct Bits<'a> {
    buf: &'a [u8],
    bit: usize,
}

impl<'a> Bits<'a> {
    fn new(buf: &'a [u8]) -> Bits<'a> {
        Bits { buf, bit: 0 }
    }

    /// `f(n)`: `n` ≤ 32 bits, or `None` past the end.
    fn f(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.buf.get(self.bit / 8)?;
            v = (v << 1) | u32::from((byte >> (7 - (self.bit % 8))) & 1);
            self.bit += 1;
        }
        Some(v)
    }

    fn flag(&mut self) -> Option<bool> {
        Some(self.f(1)? == 1)
    }

    /// `uvlc()`.
    fn uvlc(&mut self) -> Option<u32> {
        let mut leading = 0u32;
        while !self.flag()? {
            leading += 1;
            if leading >= 32 {
                return Some(u32::MAX);
            }
        }
        Some(self.f(leading)?.saturating_add((1u32 << leading) - 1))
    }
}

/// `tg_end` — the index of the last tile in a tile-group OBU's payload
/// (`payload_start..obu_end`), for a frame of `num_tiles` tiles whose tile-group
/// fields are `tile_bits = TileColsLog2 + TileRowsLog2` wide.
///
/// With one tile there are no fields and the group covers it. Returns
/// `num_tiles` (never a valid last index) when the payload is too short to hold
/// the fields, so a malformed group can never falsely complete a frame.
pub fn tile_group_tg_end(
    buf: &[u8],
    payload_start: usize,
    obu_end: usize,
    num_tiles: u32,
    tile_bits: u32,
) -> u32 {
    if num_tiles <= 1 {
        return 0;
    }
    let Some(payload) = buf.get(payload_start..obu_end.min(buf.len())) else {
        return num_tiles;
    };
    let mut r = Bits::new(payload);
    let fields = (|| {
        // tile_start_and_end_present_flag: 0 → the group covers every tile.
        if !r.flag()? {
            return Some(num_tiles - 1);
        }
        let _tg_start = r.f(tile_bits)?;
        r.f(tile_bits)
    })();
    fields.unwrap_or(num_tiles)
}

/// Scan the complete OBUs in `buf[from..avail]`: `(emit_to, frame_complete,
/// tile_groups_scanned)`.
///
/// `emit_to` is the end of the last complete OBU — always an OBU boundary —
/// and never advances past the OBU that completes the frame (a tile group
/// reaching tile `num_tiles − 1`, or a self-contained `OBU_FRAME`), so bytes the
/// encoder re-reports after the frame are never included. An OBU without a
/// size field stops the scan: amid a still-growing buffer it cannot be bounded.
pub fn scan_frame(
    buf: &[u8],
    from: usize,
    avail: usize,
    num_tiles: u32,
    tile_bits: u32,
) -> (usize, bool, u32) {
    let mut pos = from;
    let mut tile_groups = 0u32;
    loop {
        let Some(obu) = obu_header(buf, pos, avail) else {
            return (pos, false, tile_groups);
        };
        if !obu.has_size {
            return (pos, false, tile_groups);
        }
        if obu.obu_type == OBU_FRAME {
            return (obu.end, true, tile_groups);
        }
        if obu.obu_type == OBU_TILE_GROUP {
            tile_groups += 1;
            let tg_end = tile_group_tg_end(buf, obu.payload_start, obu.end, num_tiles, tile_bits);
            if tg_end + 1 >= num_tiles {
                return (obu.end, true, tile_groups);
            }
        }
        pos = obu.end;
    }
}

/// What the server needs from a sequence header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SequenceHeader {
    pub seq_profile: u8,
    /// `seq_level_idx[0]`.
    pub seq_level_idx: u8,
    /// `seq_tier[0]`.
    pub seq_tier: u8,
    pub use_128x128_superblock: bool,
}

/// Parse the first sequence-header OBU in `obus` (a sequence of OBUs with size
/// fields, as the encoder returns it). `None` if there is none or it is short.
pub fn parse_sequence_header(obus: &[u8]) -> Option<SequenceHeader> {
    let mut pos = 0;
    let obu = loop {
        let obu = obu_header(obus, pos, obus.len())?;
        if obu.obu_type == OBU_SEQUENCE_HEADER {
            break obu;
        }
        if !obu.has_size {
            return None;
        }
        pos = obu.end;
    };
    let mut r = Bits::new(obus.get(obu.payload_start..obu.end)?);

    let seq_profile = r.f(3)? as u8;
    let _still_picture = r.flag()?;
    let reduced = r.flag()?;
    let (seq_level_idx, seq_tier) = if reduced {
        (r.f(5)? as u8, 0)
    } else {
        let timing_info_present = r.flag()?;
        let mut decoder_model_info_present = false;
        let mut buffer_delay_length = 0;
        if timing_info_present {
            let _num_units_in_display_tick = r.f(32)?;
            let _time_scale = r.f(32)?;
            if r.flag()? {
                let _num_ticks_per_picture_minus_1 = r.uvlc()?;
            }
            decoder_model_info_present = r.flag()?;
            if decoder_model_info_present {
                buffer_delay_length = r.f(5)? + 1;
                let _num_units_in_decoding_tick = r.f(32)?;
                let _buffer_removal_time_length_minus_1 = r.f(5)?;
                let _frame_presentation_time_length_minus_1 = r.f(5)?;
            }
        }
        let initial_display_delay_present = r.flag()?;
        let operating_points = r.f(5)? + 1;
        let mut first = None;
        for _ in 0..operating_points {
            let _operating_point_idc = r.f(12)?;
            let level = r.f(5)? as u8;
            let tier = if level > 7 { r.f(1)? as u8 } else { 0 };
            if decoder_model_info_present && r.flag()? {
                let _decoder_buffer_delay = r.f(buffer_delay_length)?;
                let _encoder_buffer_delay = r.f(buffer_delay_length)?;
                let _low_delay_mode_flag = r.flag()?;
            }
            if initial_display_delay_present && r.flag()? {
                let _initial_display_delay_minus_1 = r.f(4)?;
            }
            first.get_or_insert((level, tier));
        }
        first?
    };
    let width_bits = r.f(4)? + 1;
    let height_bits = r.f(4)? + 1;
    let _max_frame_width_minus_1 = r.f(width_bits)?;
    let _max_frame_height_minus_1 = r.f(height_bits)?;
    if !reduced && r.flag()? {
        // frame_id_numbers_present_flag
        let _delta_frame_id_length_minus_2 = r.f(4)?;
        let _additional_frame_id_length_minus_1 = r.f(3)?;
    }
    let use_128x128_superblock = r.flag()?;
    Some(SequenceHeader {
        seq_profile,
        seq_level_idx,
        seq_tier,
        use_128x128_superblock,
    })
}

/// `MaxTiles` and `MaxTileCols` for a `seq_level_idx` (AV1 Annex A.3). Index 31
/// is "no level constraint".
pub fn level_tile_limits(seq_level_idx: u8) -> (u32, u32) {
    match seq_level_idx {
        0..=3 => (8, 4),      // 2.x
        4..=7 => (16, 6),     // 3.x
        8..=11 => (32, 8),    // 4.x
        12..=15 => (64, 8),   // 5.x
        16..=19 => (128, 16), // 6.x
        20..=23 => (256, 32), // 7.x
        _ => (MAX_TILE_COLS * MAX_TILE_ROWS, MAX_TILE_COLS),
    }
}

const MAX_TILE_WIDTH: u32 = 4096;
const MAX_TILE_AREA: u32 = 4096 * 2304;
const MAX_TILE_COLS: u32 = 64;
const MAX_TILE_ROWS: u32 = 64;

/// `tile_log2(blkSize, target)`: the smallest `k` with `blkSize << k >= target`.
fn tile_log2(blk: u32, target: u32) -> u32 {
    let mut k = 0;
    while (u64::from(blk) << k) < u64::from(target) {
        k += 1;
    }
    k
}

/// A frame's superblock geometry and the spec's uniform-tiling limits.
struct Geometry {
    sb_cols: u32,
    sb_rows: u32,
    min_log2_cols: u32,
    max_log2_cols: u32,
    max_log2_rows: u32,
    min_log2_tiles: u32,
}

impl Geometry {
    fn new(width: u32, height: u32, sb128: bool) -> Geometry {
        let mi_cols = 2 * ((width + 7) >> 3);
        let mi_rows = 2 * ((height + 7) >> 3);
        let sb_shift = if sb128 { 5 } else { 4 };
        let sb_cols = (mi_cols + (1 << sb_shift) - 1) >> sb_shift;
        let sb_rows = (mi_rows + (1 << sb_shift) - 1) >> sb_shift;
        let sb_size = sb_shift + 2;
        let max_tile_width_sb = MAX_TILE_WIDTH >> sb_size;
        let max_tile_area_sb = MAX_TILE_AREA >> (2 * sb_size);
        let min_log2_cols = tile_log2(max_tile_width_sb, sb_cols);
        Geometry {
            sb_cols,
            sb_rows,
            min_log2_cols,
            max_log2_cols: tile_log2(1, sb_cols.min(MAX_TILE_COLS)),
            max_log2_rows: tile_log2(1, sb_rows.min(MAX_TILE_ROWS)),
            min_log2_tiles: min_log2_cols.max(tile_log2(max_tile_area_sb, sb_rows * sb_cols)),
        }
    }

    fn min_log2_rows(&self, cols_log2: u32) -> u32 {
        self.min_log2_tiles.saturating_sub(cols_log2)
    }
}

/// A uniform tile grid as the decoder derives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileGrid {
    /// `TileColsLog2` / `TileRowsLog2` — what the frame header signals, and the
    /// width of each tile-group field.
    pub cols_log2: u32,
    pub rows_log2: u32,
    /// The real counts, which uniform spacing can make smaller than `2^log2`.
    pub tile_cols: u32,
    pub tile_rows: u32,
}

impl TileGrid {
    pub fn num_tiles(&self) -> u32 {
        self.tile_cols * self.tile_rows
    }

    /// The width of `tg_start`/`tg_end` in a tile-group OBU.
    pub fn tile_bits(&self) -> u32 {
        self.cols_log2 + self.rows_log2
    }
}

/// The grid a decoder derives for a `width`×`height` frame signalled with
/// uniform spacing at `cols_log2`/`rows_log2` (each clamped to the spec's range,
/// as an encoder must).
pub fn uniform_grid(
    width: u32,
    height: u32,
    sb128: bool,
    cols_log2: u32,
    rows_log2: u32,
) -> TileGrid {
    let g = Geometry::new(width, height, sb128);
    let cols_log2 = cols_log2.clamp(g.min_log2_cols, g.max_log2_cols.max(g.min_log2_cols));
    let min_rows = g.min_log2_rows(cols_log2);
    let rows_log2 = rows_log2.clamp(min_rows, g.max_log2_rows.max(min_rows));
    let tile_w = (g.sb_cols + (1 << cols_log2) - 1) >> cols_log2;
    let tile_h = (g.sb_rows + (1 << rows_log2) - 1) >> rows_log2;
    TileGrid {
        cols_log2,
        rows_log2,
        tile_cols: g.sb_cols.div_ceil(tile_w.max(1)),
        tile_rows: g.sb_rows.div_ceil(tile_h.max(1)),
    }
}

/// Choose a grid for about `units` tiles per frame: `units` rounded down to a
/// power of two, split evenly with any odd factor going to rows (horizontal
/// bands, which finish top to bottom like HEVC slices), within the spec's range
/// and the level's `MaxTileCols`/`MaxTiles`.
pub fn plan_tiles(width: u32, height: u32, units: u32, sb128: bool, seq_level_idx: u8) -> TileGrid {
    let g = Geometry::new(width, height, sb128);
    let (max_tiles, max_tile_cols) = level_tile_limits(seq_level_idx);
    let units = units.clamp(1, max_tiles);
    let k = 31 - units.leading_zeros();

    let mut cols_log2 = (k / 2).clamp(g.min_log2_cols, g.max_log2_cols.max(g.min_log2_cols));
    while (1u32 << cols_log2) > max_tile_cols && cols_log2 > g.min_log2_cols {
        cols_log2 -= 1;
    }
    // Whatever the columns could not take goes to rows.
    let mut rows_log2 = k.saturating_sub(cols_log2);
    let mut grid = uniform_grid(width, height, sb128, cols_log2, rows_log2);
    while grid.num_tiles() > max_tiles && rows_log2 > g.min_log2_rows(cols_log2) {
        rows_log2 -= 1;
        grid = uniform_grid(width, height, sb128, cols_log2, rows_log2);
    }
    grid
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NVENC's AV1 sequence-header OBU from a real 3840×2160 10-bit capture on
    /// the RTX 4070.
    const NVENC_4K_SEQUENCE_HEADER: [u8; 17] = [
        0x0a, 0x0f, 0x00, 0x00, 0x00, 0x6e, 0xef, 0xbf, 0xe1, 0xbc, 0x02, 0x19, 0xd0, 0x91, 0x00,
        0x90, 0x40,
    ];

    /// One OBU with a size field.
    fn obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![(obu_type << 3) | 0b10];
        let mut size = payload.len() as u64;
        loop {
            let mut byte = (size & 0x7f) as u8;
            size >>= 7;
            if size != 0 {
                byte |= 0x80;
            }
            v.push(byte);
            if size == 0 {
                break;
            }
        }
        v.extend_from_slice(payload);
        v
    }

    /// A tile group covering exactly tile `idx`, with `bits`-wide fields.
    fn tile_group(idx: u32, bits: u32) -> Vec<u8> {
        // flag=1, tg_start=idx, tg_end=idx, MSB-first, then filler.
        let mut acc: u64 = 1;
        acc = (acc << bits) | u64::from(idx);
        acc = (acc << bits) | u64::from(idx);
        let used = 1 + 2 * bits;
        let pad = (8 - used % 8) % 8;
        acc <<= pad;
        let nbytes = ((used + pad) / 8) as usize;
        let mut p: Vec<u8> = (0..nbytes).rev().map(|i| (acc >> (8 * i)) as u8).collect();
        p.extend_from_slice(&[0u8; 7]);
        obu(OBU_TILE_GROUP, &p)
    }

    #[test]
    fn leb128_reads_single_and_multi_byte() {
        assert_eq!(read_leb128(&[0x00], 0, 1), Some((0, 1)));
        assert_eq!(read_leb128(&[0x7f], 0, 1), Some((127, 1)));
        assert_eq!(read_leb128(&[0xc8, 0x01], 0, 2), Some((200, 2)));
        assert_eq!(read_leb128(&[0x80], 0, 1), None, "not yet arrived");
    }

    #[test]
    fn tg_end_decodes_the_real_nvenc_bytes() {
        // The four tile groups of a real 2×2 NVENC frame lead with these bytes.
        for (byte, want) in [(0x80u8, 0u32), (0xa8, 1), (0xd0, 2), (0xf8, 3)] {
            let buf = [byte, 0, 0, 0];
            assert_eq!(tile_group_tg_end(&buf, 0, buf.len(), 4, 2), want);
        }
    }

    #[test]
    fn scan_stops_at_the_last_tile_and_ignores_trailing_bytes() {
        let mut frame = Vec::new();
        frame.extend(obu(2, &[]));
        frame.extend(NVENC_4K_SEQUENCE_HEADER);
        frame.extend(obu(3, &[9, 9, 9, 9]));
        for idx in 0..4 {
            frame.extend(tile_group(idx, 2));
        }
        let real_end = frame.len();
        // What the encoder re-reports after the frame: the same tiles again.
        for idx in 0..4 {
            frame.extend(tile_group(idx, 2));
        }
        let (emit_to, complete, groups) = scan_frame(&frame, 0, frame.len(), 4, 2);
        assert!(complete);
        assert_eq!(emit_to, real_end, "must stop at the real frame end");
        assert_eq!(groups, 4);
    }

    #[test]
    fn scan_finishes_a_56_tile_frame_on_its_real_last_tile() {
        // 4K asked for 8×8 is really 8×7 = 56 tiles with 6-bit fields. The old
        // drain waited for tile 63 and never saw it.
        let grid = uniform_grid(3840, 2160, false, 3, 3);
        assert_eq!((grid.num_tiles(), grid.tile_bits()), (56, 6));
        let mut frame = obu(3, &[1, 2, 3]);
        for idx in 0..56 {
            frame.extend(tile_group(idx, 6));
        }
        let real_end = frame.len();
        frame.extend(tile_group(0, 6));
        let (emit_to, complete, groups) =
            scan_frame(&frame, 0, frame.len(), grid.num_tiles(), grid.tile_bits());
        assert!(complete, "the frame completes on tile 55");
        assert_eq!((emit_to, groups), (real_end, 56));
    }

    #[test]
    fn scan_waits_for_a_partial_obu() {
        let mut frame = obu(3, &[9, 9]);
        frame.extend(tile_group(0, 2));
        let after = frame.len();
        frame.extend(tile_group(1, 2));
        let (emit_to, complete, _) = scan_frame(&frame, 0, after + 3, 4, 2);
        assert!(!complete);
        assert_eq!(emit_to, after, "only through the last complete OBU");
    }

    #[test]
    fn parses_nvencs_real_sequence_header() {
        let seq = parse_sequence_header(&NVENC_4K_SEQUENCE_HEADER).expect("parses");
        assert_eq!(
            seq,
            SequenceHeader {
                seq_profile: 0,
                seq_level_idx: 13,
                // High tier — which the av1C record used to claim was Main.
                seq_tier: 1,
                use_128x128_superblock: false,
            }
        );
    }

    #[test]
    fn a_sequence_header_is_found_behind_other_obus() {
        let mut obus = obu(2, &[]);
        obus.extend(NVENC_4K_SEQUENCE_HEADER);
        assert_eq!(
            parse_sequence_header(&obus).map(|s| s.seq_level_idx),
            Some(13)
        );
        assert_eq!(parse_sequence_header(&obu(2, &[])), None);
        assert_eq!(parse_sequence_header(&NVENC_4K_SEQUENCE_HEADER[..6]), None);
    }

    #[test]
    fn plans_the_4k_grid_the_spec_actually_produces() {
        // (units, tile_cols, tile_rows, tile_bits) at 3840×2160, 64 px
        // superblocks (60×34), level 5.1.
        for (units, cols, rows, bits) in [
            (1, 1, 1, 0),
            (2, 1, 2, 1),
            (3, 1, 2, 1),
            (4, 2, 2, 2),
            (8, 2, 4, 3),
            (16, 4, 4, 4),
            (32, 4, 7, 5),
            (64, 8, 7, 6),
            // Level 5.1 allows 64 tiles and 8 columns; 128 asks for more.
            (128, 8, 7, 6),
        ] {
            let g = plan_tiles(3840, 2160, units, false, 13);
            assert_eq!(
                (g.tile_cols, g.tile_rows, g.tile_bits()),
                (cols, rows, bits),
                "{units} units at 4K"
            );
        }
    }

    #[test]
    fn plans_within_the_level_at_1080p() {
        // 1920×1080: 30×17 superblocks; level 4.0 allows 32 tiles, 8 columns.
        assert_eq!(plan_tiles(1920, 1080, 4, false, 8).num_tiles(), 4);
        let g = plan_tiles(1920, 1080, 16, false, 8);
        assert_eq!((g.tile_cols, g.tile_rows), (4, 4));
        let g = plan_tiles(1920, 1080, 64, false, 8);
        assert_eq!(
            (g.tile_cols, g.tile_rows),
            (4, 6),
            "clamped to 32, spaced to 24"
        );
        assert!(g.num_tiles() <= 32);
    }

    #[test]
    fn level_limits_follow_annex_a() {
        assert_eq!(level_tile_limits(8), (32, 8));
        assert_eq!(level_tile_limits(13), (64, 8));
        assert_eq!(level_tile_limits(16), (128, 16));
    }

    #[test]
    fn superblock_size_changes_the_grid() {
        // 128 px superblocks: 4K is 30×17, so 8 rows are really 6.
        let g = uniform_grid(3840, 2160, true, 3, 3);
        assert_eq!((g.tile_cols, g.tile_rows), (8, 6));
    }
}

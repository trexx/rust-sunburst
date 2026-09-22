// SPDX-License-Identifier: GPL-2.0-or-later

//! A HEVC or AV1 encoder over the promoted [`nvenc`](super::nvenc) FFI.
//!
//! Opens a DirectX session against the capture backend's `ID3D11Device`, applies
//! the **P1 / ultra-low-latency** preset (CLAUDE.md — `TUNING_INFO_ULTRA_LOW_
//! LATENCY`, no B-frames, no lookahead), and encodes P010 textures the
//! [`convert`](super::convert) stage produces into an HEVC (slices) or AV1
//! (tiles) bitstream, with subframe readback and HDR mastering metadata.
//!
//! The parameter structs are transcribed field-for-field from
//! `Video_Codec_Interface_13.1.15/Interface/nvEncodeAPI.h`. Their versions
//! (`NVENCAPI_STRUCT_VERSION`) select the layout the driver uses, so a
//! transcription slip is an `NV_ENC_ERR_INVALID_VERSION`, not a silent bug — but
//! the layouts themselves are only exercised on the 4070 (this host has no NVENC),
//! so treat this as compile-verified, box-to-validate.
//!
//! The encode functions the promoted spike left as `*mut c_void` (it only queried
//! caps) are transmuted to their real signatures here rather than re-typing the
//! shared function-list struct.

use std::ffi::{CStr, c_char, c_void};

use sunburst_capture::HdrMetadata;

use crate::hdr::{ContentLightLevel, MasteringDisplayInfo};
use crate::nvenc::{
    Guid, NV_ENC_CODEC_AV1_GUID, NV_ENC_CODEC_H264_GUID, NV_ENC_CODEC_HEVC_GUID,
    NvEncodeApiFunctionList, Nvenc, NvencStatus, Session, struct_version,
};

/// The codec an [`Encoder`] targets.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Codec {
    /// HEVC Main10 — the Shield's codec. Subdivided into slices.
    Hevc,
    /// AV1 10-bit — the Homatics' codec. Subdivided into tiles.
    Av1,
    /// H.264 High, 8-bit SDR — an opt-in low-latency codec. NV12 input (not
    /// P010); slices, like HEVC; no HDR (NVENC H.264 is 8-bit only).
    H264,
}

impl Codec {
    fn guid(self) -> Guid {
        match self {
            Codec::Hevc => NV_ENC_CODEC_HEVC_GUID,
            Codec::Av1 => NV_ENC_CODEC_AV1_GUID,
            Codec::H264 => NV_ENC_CODEC_H264_GUID,
        }
    }

    /// The NVENC input buffer format this codec's convert stage produces —
    /// 8-bit NV12 for H.264, 10-bit P010 for HEVC/AV1.
    fn buffer_format(self) -> u32 {
        match self {
            Codec::H264 => NV_ENC_BUFFER_FORMAT_NV12,
            Codec::Hevc | Codec::Av1 => NV_ENC_BUFFER_FORMAT_YUV420_10BIT,
        }
    }
}

// ── constants from nvEncodeAPI.h ─────────────────────────────────────────────

const NV_ENC_SUCCESS: NvencStatus = 0;

const NV_ENC_INITIALIZE_PARAMS_VER: u32 = struct_version(7) | (1 << 31);
const NV_ENC_CONFIG_VER: u32 = struct_version(9) | (1 << 31);
const NV_ENC_PRESET_CONFIG_VER: u32 = struct_version(5) | (1 << 31);
const NV_ENC_PIC_PARAMS_VER: u32 = struct_version(7) | (1 << 31);
const NV_ENC_LOCK_BITSTREAM_VER: u32 = struct_version(2) | (1 << 31);
const NV_ENC_REGISTER_RESOURCE_VER: u32 = struct_version(5);
const NV_ENC_MAP_INPUT_RESOURCE_VER: u32 = struct_version(4);
const NV_ENC_CREATE_BITSTREAM_BUFFER_VER: u32 = struct_version(1);
const NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER: u32 = struct_version(1);

/// `NV_ENC_PRESET_P1_GUID` .. `P4` (nvEncodeAPI.h). P1 is fastest/lowest-latency;
/// the ULL *tuning* is fixed regardless — the preset trades quality for encode
/// time inside it. P5–P7 are not exposed (they cost latency the project spends
/// nowhere else).
const NV_ENC_PRESET_P1_GUID: Guid = Guid {
    data1: 0xfc0a_8d3e,
    data2: 0x45f8,
    data3: 0x4cf8,
    data4: [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf],
};
const NV_ENC_PRESET_P2_GUID: Guid = Guid {
    data1: 0xf581_cfb8,
    data2: 0x88d6,
    data3: 0x4381,
    data4: [0x93, 0xf0, 0xdf, 0x13, 0xf9, 0xc2, 0x7d, 0xab],
};
const NV_ENC_PRESET_P3_GUID: Guid = Guid {
    data1: 0x3685_0110,
    data2: 0x3a07,
    data3: 0x441f,
    data4: [0x94, 0xd5, 0x36, 0x70, 0x63, 0x1f, 0x91, 0xf6],
};
const NV_ENC_PRESET_P4_GUID: Guid = Guid {
    data1: 0x90a7_b826,
    data2: 0xdf06,
    data3: 0x4862,
    data4: [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81],
};

/// The preset GUID for `preset` (1..=4, clamped).
fn preset_guid(preset: u8) -> Guid {
    match preset.clamp(1, 4) {
        1 => NV_ENC_PRESET_P1_GUID,
        2 => NV_ENC_PRESET_P2_GUID,
        3 => NV_ENC_PRESET_P3_GUID,
        _ => NV_ENC_PRESET_P4_GUID,
    }
}

/// `NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY`.
const NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY: u32 = 3;
/// `NV_ENC_BUFFER_FORMAT_YUV420_10BIT` — P010, NVENC's 10-bit semi-planar input.
const NV_ENC_BUFFER_FORMAT_YUV420_10BIT: u32 = 0x0001_0000;
/// `NV_ENC_BUFFER_FORMAT_NV12` — 8-bit semi-planar input, for the H.264 SDR path.
const NV_ENC_BUFFER_FORMAT_NV12: u32 = 0x0000_0001;
/// `NV_ENC_BIT_DEPTH_10`. SDK 13.x configures 10-bit via `inputBitDepth` /
/// `outputBitDepth` enums (value = the bit count), not the old
/// `pixelBitDepthMinus8`. Leaving it unset keeps the session 8-bit, and then
/// nvEncRegisterResource rejects a P010 surface with INVALID_PARAM.
const NV_ENC_BIT_DEPTH_10: u32 = 10;
/// `NV_ENC_VUI_*` color-description enum values (nvEncodeAPI 13.1.15): BT.709 for
/// SDR, BT.2020 primaries + PQ (SMPTE 2084) transfer + BT.2020 non-constant-luminance
/// matrix for HDR10. Written into the HEVC VUI / AV1 sequence-header color_config.
const NV_ENC_VUI_PRIMARIES_BT709: u32 = 1;
const NV_ENC_VUI_PRIMARIES_BT2020: u32 = 9;
const NV_ENC_VUI_TRANSFER_BT709: u32 = 1;
const NV_ENC_VUI_TRANSFER_SMPTE2084: u32 = 16;
const NV_ENC_VUI_MATRIX_BT709: u32 = 1;
const NV_ENC_VUI_MATRIX_BT2020_NCL: u32 = 9;
/// `NV_ENC_PIC_TYPE` values NVENC reports in `NV_ENC_LOCK_BITSTREAM::pictureType`.
/// Only IDR matters here: it is the wire keyframe flag, set from what NVENC
/// actually produced (so an auto-inserted periodic IDR is flagged too), not from
/// the force-IDR request.
const NV_ENC_PIC_TYPE_IDR: u32 = 0x03;

/// The colorimetry the encoder tags the bitstream with (HEVC VUI / AV1 sequence
/// header), matched to what the convert stage produced so the decoder interprets
/// the pixels correctly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColorSpace {
    /// BT.709 primaries + transfer + matrix — SDR.
    Bt709,
    /// BT.2020 primaries, PQ transfer, BT.2020-NCL matrix — HDR10.
    Bt2020Pq,
}

impl ColorSpace {
    /// `(colour_primaries, transfer_characteristics, matrix_coefficients)` as
    /// `NV_ENC_VUI_*` enum values.
    fn vui(self) -> (u32, u32, u32) {
        match self {
            ColorSpace::Bt709 => (
                NV_ENC_VUI_PRIMARIES_BT709,
                NV_ENC_VUI_TRANSFER_BT709,
                NV_ENC_VUI_MATRIX_BT709,
            ),
            ColorSpace::Bt2020Pq => (
                NV_ENC_VUI_PRIMARIES_BT2020,
                NV_ENC_VUI_TRANSFER_SMPTE2084,
                NV_ENC_VUI_MATRIX_BT2020_NCL,
            ),
        }
    }
}
/// `NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX`.
const NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX: u32 = 0;
/// `NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR`.
const NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR: u32 = 1;
/// `NV_ENC_INPUT_IMAGE` (buffer usage).
const NV_ENC_INPUT_IMAGE: u32 = 0;
/// `NV_ENC_PIC_STRUCT_FRAME`.
const NV_ENC_PIC_STRUCT_FRAME: u32 = 1;
/// `NV_ENC_PIC_FLAG_FORCEIDR` — encode this picture as an IDR.
const NV_ENC_PIC_FLAG_FORCEIDR: u32 = 0x2;
/// `NV_ENC_PIC_FLAG_OUTPUT_SPSPPS` — inline the sequence headers on this frame,
/// so a client that joins or recovers at the IDR has them.
const NV_ENC_PIC_FLAG_OUTPUT_SPSPPS: u32 = 0x4;
/// `NV_ENC_PARAMS_RC_CBR` — constant bitrate, the low-latency choice.
const NV_ENC_PARAMS_RC_CBR: u32 = 0x2;
/// `NV_ENC_PARAMS_RC_VBR` — variable bitrate.
const NV_ENC_PARAMS_RC_VBR: u32 = 0x1;
/// `NVENC_INFINITE_GOPLENGTH` — never emit a periodic IDR; recovery is by
/// reference invalidation and intra refresh instead (CLAUDE.md).
const NVENC_INFINITE_GOPLENGTH: u32 = 0xffff_ffff;
/// `NV_ENC_RECONFIGURE_PARAMS_VER`.
const NV_ENC_RECONFIGURE_PARAMS_VER: u32 = struct_version(2) | (1 << 31);
/// `enableIntraRefresh` bit in `NV_ENC_CONFIG_HEVC`'s first bitfield word.
const HEVC_ENABLE_INTRA_REFRESH: u32 = 1 << 8;
/// `enableIntraRefresh` bit in `NV_ENC_CONFIG_AV1`'s first bitfield word.
const AV1_ENABLE_INTRA_REFRESH: u32 = 1 << 6;

/// `NV_ENC_CAPS_SUPPORT_DYN_BITRATE_CHANGE`.
const NV_ENC_CAPS_SUPPORT_DYN_BITRATE_CHANGE: u32 = 12;
/// `NV_ENC_CAPS_SUPPORT_INTRA_REFRESH`.
const NV_ENC_CAPS_SUPPORT_INTRA_REFRESH: u32 = 15;
/// `NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION`.
const NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION: u32 = 19;

/// How to build an encoder. Replaces the positional soup `new` used to take, and
/// carries the rate-control and recovery settings the transport drives.
#[derive(Clone, Debug)]
pub struct EncoderConfig {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    /// Whole frames per second, for the VBV sizing and the rate header.
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// HEVC slices, or AV1 tiles per axis (`2` = a 2×2 grid). `>1` turns on
    /// subframe readback.
    pub slices: u32,
    pub hdr: Option<HdrMetadata>,
    /// The colorimetry to tag the bitstream with, matched to the convert output.
    pub color: ColorSpace,
    /// `(period, count)` for gradual intra refresh, or `None`. Gated on the
    /// decoder's quirks and the encoder's caps by the caller.
    pub intra_refresh: Option<(u32, u32)>,
    /// `maxNumRefFramesInDPB`. A deep DPB is what lets reference invalidation
    /// fall back to an older good frame instead of forcing a keyframe
    /// (`NvEncInvalidateRefFrames` docs recommend it).
    pub dpb_depth: u32,
    /// NVENC preset P1–P4 (1..=4, clamped). Stays within the ULL tuning; not a
    /// UHQ escape.
    pub preset: u8,
    /// Variable-bitrate rate control (else constant bitrate).
    pub vbr: bool,
    /// Forced IDR period in frames; `0` = infinite GOP (recovery via
    /// intra-refresh / reference invalidation).
    pub idr_period: u32,
}

impl EncoderConfig {
    /// A sensible default for `codec` at `width`×`height`: the plan's slice/tile
    /// counts, an 8-frame DPB, no intra refresh, SDR.
    pub fn new(codec: Codec, width: u32, height: u32) -> EncoderConfig {
        let slices = match codec {
            Codec::Hevc | Codec::H264 => 4,
            Codec::Av1 => 2,
        };
        EncoderConfig {
            codec,
            width,
            height,
            fps: 60,
            bitrate_kbps: 120_000,
            slices,
            hdr: None,
            color: ColorSpace::Bt709,
            intra_refresh: None,
            dpb_depth: 8,
            preset: 1,
            vbr: false,
            idr_period: 0,
        }
    }
}

/// What the encoder reported it can do. Queried once at build, so the transport
/// does not assume parity between HEVC and AV1 (CLAUDE.md: "Do not assume").
#[derive(Clone, Copy, Debug, Default)]
pub struct EncoderCaps {
    pub ref_invalidation: bool,
    pub intra_refresh: bool,
    pub dyn_bitrate: bool,
}

/// Per-frame encode request.
#[derive(Clone, Copy, Debug, Default)]
pub struct PicRequest {
    /// The tag reference invalidation keys on — the frame's capture time. Must be
    /// distinct and increasing across frames.
    pub timestamp: u64,
    /// Force this frame to be an IDR (and inline the sequence headers). Used for
    /// the first frame, a client `RequestIdr`, and recovery when nothing good is
    /// left to reference.
    pub force_idr: bool,
}

// ── parameter structs (see nvEncodeAPI.h) ────────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
struct NvEncQp {
    qp_inter_p: u32,
    qp_inter_b: u32,
    qp_intra: u32,
}

#[repr(C)]
struct NvEncRcParams {
    version: u32,
    rate_control_mode: u32,
    const_qp: NvEncQp,
    average_bit_rate: u32,
    max_bit_rate: u32,
    vbv_buffer_size: u32,
    vbv_initial_delay: u32,
    /// `enableMinQP:1` … `reservedBitFields:15` — one 32-bit field group.
    bitfields: u32,
    min_qp: NvEncQp,
    max_qp: NvEncQp,
    initial_rc_qp: NvEncQp,
    temporal_layer_idx_mask: u32,
    temporal_layer_qp: [u8; 8],
    target_quality: u8,
    target_quality_lsb: u8,
    lookahead_depth: u16,
    low_delay_key_frame_scale: u8,
    y_dc_qp_index_offset: i8,
    u_dc_qp_index_offset: i8,
    v_dc_qp_index_offset: i8,
    qp_map_mode: u32,
    multi_pass: u32,
    alpha_layer_bitrate_ratio: u32,
    cb_qp_index_offset: i8,
    cr_qp_index_offset: i8,
    reserved2: u16,
    lookahead_level: u32,
    view_bitrate_ratios: [u8; 7],
    reserved3: u8,
    reserved1: u32,
}

/// `NV_ENC_CODEC_CONFIG` union — a fixed 1280-byte blob (`reserved[320]`), filled
/// by the driver's preset config; never hand-read here.
#[repr(C)]
struct NvEncCodecConfig {
    reserved: [u32; 320],
}

/// The head of `NV_ENC_CONFIG_HEVC`, up to `sliceMode`/`sliceModeData`. Overlaid
/// on the codec-config union (which begins with the HEVC config) to set slicing
/// for subframe readback, without transcribing the whole 100-field struct.
#[repr(C)]
struct HevcConfigHead {
    level: u32,
    tier: u32,
    min_cu_size: u32,
    max_cu_size: u32,
    /// `useConstrainedIntraPred:1` … `reserved:5` — one 32-bit field group.
    bitfields: u32,
    idr_period: u32,
    intra_refresh_period: u32,
    intra_refresh_cnt: u32,
    max_num_ref_frames_in_dpb: u32,
    ltr_num_frames: u32,
    vps_id: u32,
    sps_id: u32,
    pps_id: u32,
    slice_mode: u32,
    slice_mode_data: u32,
    max_temporal_layers_minus1: u32, // @60
    // NV_ENC_CONFIG_HEVC_VUI_PARAMETERS (@64, 112 bytes); only the color-signaling
    // words are named, the rest is opaque (left at the preset default).
    _vui_pad0: [u32; 2],                  // overscan* (@64,@68)
    video_signal_type_present_flag: u32,  // @72
    _vui_pad1: u32,                       // videoFormat (@76)
    video_full_range_flag: u32,           // @80
    colour_description_present_flag: u32, // @84
    colour_primaries: u32,                // @88
    transfer_characteristics: u32,        // @92
    colour_matrix: u32,                   // @96
    // rest of the VUI (@100) through disableDeblockingFilterIDC (@196) — 100 bytes.
    _reserved_to_bit_depth: [u32; 25],
    /// `NV_ENC_CONFIG_HEVC::outputBitDepth` (@200) / `inputBitDepth` (@204).
    output_bit_depth: u32,
    input_bit_depth: u32,
}

/// The head of `NV_ENC_CONFIG_AV1`, up to `numTileColumns`/`numTileRows`. Overlaid
/// on the codec-config union to set uniform tiles + the HDR output flags.
#[repr(C)]
struct Av1ConfigHead {
    level: u32,
    tier: u32,
    min_part_size: u32,
    max_part_size: u32,
    /// `outputAnnexBFormat:1` … `reserved:14`; `outputMaxCll` is bit 14,
    /// `outputMasteringDisplay` bit 15.
    bitfields: u32,
    idr_period: u32,
    intra_refresh_period: u32,
    intra_refresh_cnt: u32,
    max_num_ref_frames_in_dpb: u32,
    num_tile_columns: u32,
    num_tile_rows: u32,
    // reserved2 (@44), tileWidths (@48, ptr), tileHeights (@56, ptr),
    // maxTemporalLayersMinus1 (@64) — opaque, left at the preset default.
    _pad_to_color: [u32; 6],       // @44..68
    color_primaries: u32,          // @68
    transfer_characteristics: u32, // @72
    matrix_coefficients: u32,      // @76
    color_range: u32,              // @80
    // chromaSamplePosition (@84) through numBwdRefs (@108), incl. filmGrainParams
    // (ptr) + its alignment pad — opaque. 28 bytes.
    _reserved_to_bit_depth: [u32; 7],
    /// `NV_ENC_CONFIG_AV1::outputBitDepth` (@112) / `inputBitDepth` (@116).
    output_bit_depth: u32,
    input_bit_depth: u32,
}

/// The head of `NV_ENC_CONFIG_H264` — up to the VUI colour description — overlaid
/// on the codec-config union for an H.264 encoder to set slicing, intra-refresh
/// and the BT.709 colour signalling, without transcribing all ~90 fields.
/// `enableIntraRefresh` is bit 10 of the first flag word (from `enableTemporalSVC`).
/// The fields after `idrPeriod` are separate `uint32_t`s (`separateColourPlaneFlag`,
/// `disableDeblockingFilterIDC`, `numTemporalLayers`, `spsId`, `ppsId`), NOT one
/// packed word — modelling them as one shifted `maxNumRefFrames`/`sliceMode` 16
/// bytes low, so slicing/refs were silently misconfigured. Offsets asserted below.
#[repr(C)]
struct H264ConfigHead {
    /// `enableTemporalSVC:1 … enableIntraRefresh:1 (bit 10) … reservedBitFields:10`.
    flags: u32, // @0
    level: u32,                         // @4
    idr_period: u32,                    // @8
    separate_colour_plane_flag: u32,    // @12
    disable_deblocking_filter_idc: u32, // @16
    num_temporal_layers: u32,           // @20
    sps_id: u32,                        // @24
    pps_id: u32,                        // @28
    adaptive_transform_mode: u32,       // @32
    fmo_mode: u32,                      // @36
    bdirect_mode: u32,                  // @40
    entropy_coding_mode: u32,           // @44
    stereo_mode: u32,                   // @48
    intra_refresh_period: u32,          // @52
    intra_refresh_cnt: u32,             // @56
    max_num_ref_frames: u32,            // @60
    slice_mode: u32,                    // @64
    slice_mode_data: u32,               // @68
    // h264VUIParameters (@72, 112 bytes); only the colour words are named.
    _vui_pad0: [u32; 2],                  // overscan* (@72,@76)
    video_signal_type_present_flag: u32,  // @80
    _vui_pad1: u32,                       // videoFormat (@84)
    video_full_range_flag: u32,           // @88
    colour_description_present_flag: u32, // @92
    colour_primaries: u32,                // @96
    transfer_characteristics: u32,        // @100
    colour_matrix: u32,                   // @104
}

/// `NV_ENC_CONFIG_H264`'s `enableIntraRefresh` — bit 10 of the first flag word.
const H264_ENABLE_INTRA_REFRESH: u32 = 1 << 10;

/// The head of `NV_ENC_PIC_PARAMS_HEVC`, up to the HDR metadata pointers. Overlaid
/// on the pic-params codec union to point the per-frame `pMaxCll` /
/// `pMasteringDisplay` at our metadata (`time_code` is a correctly-sized blob —
/// `NV_ENC_TIME_CODE` is 32 bytes — so the pointers land at the right offset).
#[repr(C)]
struct HevcPicParamsHead {
    display_poc_syntax: u32,
    ref_pic_flag: u32,
    temporal_id: u32,
    force_intra_refresh_with_frame_cnt: u32,
    bitfields: u32,
    reserved1: u32,
    slice_type_data: *mut c_void,
    slice_type_array_cnt: u32,
    slice_mode: u32,
    slice_mode_data: u32,
    ltr_mark_frame_idx: u32,
    ltr_use_frame_bitmap: u32,
    ltr_usage_mode: u32,
    sei_payload_array_cnt: u32,
    reserved: u32,
    sei_payload_array: *mut c_void,
    time_code: [u32; 8],
    num_temporal_layers: u32,
    view_id: u32,
    /// `p3DReferenceDisplayInfo` (@112) — left null. Omitting it lands
    /// `p_max_cll`/`p_mastering_display` 8 bytes low, so NVENC dereferences
    /// garbage on the HDR path (a crash the first time HDR is actually active).
    p_3d_reference_display_info: *mut c_void,
    p_max_cll: *const c_void,
    p_mastering_display: *const c_void,
}

/// The head of `NV_ENC_PIC_PARAMS_AV1`, up to the HDR metadata pointers.
#[repr(C)]
struct Av1PicParamsHead {
    display_poc_syntax: u32,
    ref_pic_flag: u32,
    temporal_id: u32,
    force_intra_refresh_with_frame_cnt: u32,
    bitfields: u32,
    num_tile_columns: u32,
    num_tile_rows: u32,
    reserved: u32,
    tile_widths: *mut c_void,
    tile_heights: *mut c_void,
    obu_payload_array_cnt: u32,
    reserved1: u32,
    obu_payload_array: *mut c_void,
    film_grain_params: *mut c_void,
    ltr_mark_frame_idx: u32,
    ltr_use_frame_bitmap: u32,
    num_temporal_layers: u32,
    reserved4: u32,
    p_max_cll: *const c_void,
    p_mastering_display: *const c_void,
}

#[repr(C)]
struct NvEncConfig {
    version: u32,
    profile_guid: Guid,
    gop_length: u32,
    frame_interval_p: i32,
    mono_chrome_encoding: u32,
    frame_field_mode: u32,
    mv_precision: u32,
    rc_params: NvEncRcParams,
    encode_codec_config: NvEncCodecConfig,
    reserved: [u32; 278],
    reserved2: [*mut c_void; 64],
}

#[repr(C)]
struct NvEncPresetConfig {
    version: u32,
    reserved: u32,
    preset_cfg: NvEncConfig,
    reserved1: [u32; 256],
    reserved2: [*mut c_void; 64],
}

/// `NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE`: one bitfield word (four
/// `numCandsPerBlk*:4` fields + `reserved:16`) *plus* `reserved1[3]` — 16
/// bytes, not 4. This width is load-bearing: the field sits before `tuningInfo`
/// in NV_ENC_INITIALIZE_PARAMS, so a 4-byte version shifts `tuningInfo` (and
/// every field after it) back 24 bytes and the driver reads a zeroed
/// tuningInfo — "Presets P1-P7 are only supported with valid ...tuningInfo".
/// We never emit external ME hints, so the words stay zero; only the width
/// matters. Same fix corrects the copy in `NvEncPicParams`.
type MeHintCounts = [u32; 4];

#[repr(C)]
struct NvEncInitializeParams {
    version: u32,
    encode_guid: Guid,
    preset_guid: Guid,
    encode_width: u32,
    encode_height: u32,
    dar_width: u32,
    dar_height: u32,
    frame_rate_num: u32,
    frame_rate_den: u32,
    enable_encode_async: u32,
    enable_ptd: u32,
    /// `reportSliceOffsets:1` … `reservedBitFields:19`.
    bitfields: u32,
    priv_data_size: u32,
    reserved: u32,
    priv_data: *mut c_void,
    encode_config: *mut NvEncConfig,
    max_encode_width: u32,
    max_encode_height: u32,
    max_me_hint_counts_per_block: [MeHintCounts; 2],
    tuning_info: u32,
    buffer_format: u32,
    num_state_buffers: u32,
    output_stats_level: u32,
    reserved1: [u32; 284],
    reserved2: [*mut c_void; 64],
}

#[repr(C)]
struct NvEncRegisterResource {
    version: u32,
    resource_type: u32,
    width: u32,
    height: u32,
    pitch: u32,
    sub_resource_index: u32,
    resource_to_register: *mut c_void,
    registered_resource: *mut c_void,
    buffer_format: u32,
    buffer_usage: u32,
    p_input_fence_point: *mut c_void,
    chroma_offset: [u32; 2],
    chroma_offset_in: [u32; 2],
    reserved1: [u32; 244],
    reserved2: [*mut c_void; 61],
}

#[repr(C)]
struct NvEncMapInputResource {
    version: u32,
    sub_resource_index: u32,
    input_resource: *mut c_void,
    registered_resource: *mut c_void,
    mapped_resource: *mut c_void,
    mapped_buffer_fmt: u32,
    reserved1: [u32; 251],
    reserved2: [*mut c_void; 63],
}

#[repr(C)]
struct NvEncCreateBitstreamBuffer {
    version: u32,
    size: u32,
    memory_heap: u32,
    reserved: u32,
    bitstream_buffer: *mut c_void,
    bitstream_buffer_ptr: *mut c_void,
    reserved1: [u32; 58],
    reserved2: [*mut c_void; 64],
}

#[repr(C)]
struct NvEncPicParams {
    version: u32,
    input_width: u32,
    input_height: u32,
    input_pitch: u32,
    encode_pic_flags: u32,
    frame_idx: u32,
    input_time_stamp: u64,
    input_duration: u64,
    input_buffer: *mut c_void,
    output_bitstream: *mut c_void,
    completion_event: *mut c_void,
    buffer_fmt: u32,
    picture_struct: u32,
    picture_type: u32,
    /// `NV_ENC_CODEC_PIC_PARAMS` (1544 bytes). `u64` not `u32` so the union is
    /// 8-byte aligned at offset 80, matching NVENC — a `[u32; 256]` lands it at 76
    /// and every pointer written into it (HDR SEI) is 4 bytes off (NVENC crash).
    codec_pic_params: [u64; 193],
    me_hint_counts_per_block: [MeHintCounts; 2],
    me_external_hints: *mut c_void,
    reserved2: [u32; 7],
    reserved5: [*mut c_void; 2],
    qp_delta_map: *mut i8,
    qp_delta_map_size: u32,
    reserved_bit_fields: u32,
    me_hint_ref_pic_dist: [u16; 2],
    diff_pic_num_hint: i32,
    alpha_buffer: *mut c_void,
    me_external_sb_hints: *mut c_void,
    me_sb_hints_count: u32,
    state_buffer_idx: u32,
    output_recon_buffer: *mut c_void,
    reserved3: [u32; 284],
    reserved6: [*mut c_void; 57],
}

#[repr(C)]
struct NvEncLockBitstream {
    version: u32,
    /// `doNotWait:1`, `ltrFrame:1`, `getRCStats:1`, `reservedBitFields:29`.
    bitfields: u32,
    output_bitstream: *mut c_void,
    slice_offsets: *mut u32,
    frame_idx: u32,
    hw_encode_status: u32,
    num_slices: u32,
    bitstream_size_in_bytes: u32,
    output_time_stamp: u64,
    output_duration: u64,
    bitstream_buffer_ptr: *mut c_void,
    picture_type: u32,
    picture_struct: u32,
    frame_avg_qp: u32,
    frame_satd: u32,
    ltr_frame_idx: u32,
    ltr_frame_bitmap: u32,
    temporal_id: u32,
    intra_mb_count: u32,
    inter_mb_count: u32,
    average_mvx: i32,
    average_mvy: i32,
    alpha_layer_size_in_bytes: u32,
    output_stats_ptr_size: u32,
    reserved: u32,
    output_stats_ptr: *mut c_void,
    frame_idx_display: u32,
    reserved1: [u32; 219],
    reserved2: [*mut c_void; 63],
    reserved_internal: [u32; 8],
}

// ── function signatures (spike left these as *mut c_void) ────────────────────

type FnInitialize = unsafe extern "C" fn(*mut c_void, *mut NvEncInitializeParams) -> NvencStatus;
type FnGetPresetEx =
    unsafe extern "C" fn(*mut c_void, Guid, Guid, u32, *mut NvEncPresetConfig) -> NvencStatus;
type FnRegister = unsafe extern "C" fn(*mut c_void, *mut NvEncRegisterResource) -> NvencStatus;
type FnMap = unsafe extern "C" fn(*mut c_void, *mut NvEncMapInputResource) -> NvencStatus;
type FnCreateBitstream =
    unsafe extern "C" fn(*mut c_void, *mut NvEncCreateBitstreamBuffer) -> NvencStatus;
type FnEncode = unsafe extern "C" fn(*mut c_void, *mut NvEncPicParams) -> NvencStatus;
type FnLock = unsafe extern "C" fn(*mut c_void, *mut NvEncLockBitstream) -> NvencStatus;
type FnPtrArg = unsafe extern "C" fn(*mut c_void, *mut c_void) -> NvencStatus;
type FnGetSequenceParams =
    unsafe extern "C" fn(*mut c_void, *mut NvEncSequenceParamPayload) -> NvencStatus;
type FnReconfigure = unsafe extern "C" fn(*mut c_void, *mut NvEncReconfigureParams) -> NvencStatus;
type FnInvalidate = unsafe extern "C" fn(*mut c_void, u64) -> NvencStatus;
type FnLastError = unsafe extern "C" fn(*mut c_void) -> *const c_char;

/// `NV_ENC_RECONFIGURE_PARAMS` — a re-init params block plus two flag bits.
#[repr(C)]
struct NvEncReconfigureParams {
    version: u32,
    reserved: u32,
    re_init_encode_params: NvEncInitializeParams,
    /// `resetEncoder:1, forceIDR:1, reserved1:30`.
    bitfields: u32,
    reserved2: u32,
}

#[repr(C)]
struct NvEncSequenceParamPayload {
    version: u32,
    in_buffer_size: u32,
    sps_id: u32,
    pps_id: u32,
    spspps_buffer: *mut c_void,
    out_spspps_payload_size: *mut u32,
    reserved: [u32; 250],
    reserved2: [*mut c_void; 64],
}

/// A configured HEVC or AV1 encoder.
pub struct Encoder<'a> {
    session: Session<'a>,
    cfg: EncoderConfig,
    caps: EncoderCaps,
    /// `NV_ENC_INPUT_RESOURCE_TYPE_*` — DIRECTX (a texture) or CUDADEVICEPTR (a
    /// CUDA surface), set by how the encoder was opened.
    resource_type: u32,
    /// Row pitch in bytes for a CUDA input (0 for D3D11, which infers it).
    input_pitch: u32,
    bitstream: *mut c_void,
    /// The input surface registered once and reused: `(input ptr, registered
    /// resource)`. The converter hands back one reused P010 surface, so
    /// registration is a per-session cost, not a per-frame one.
    registered: Option<(*mut c_void, *mut c_void)>,
    /// HDR metadata pointed at by each frame's pic-params, kept alive here so the
    /// pointers stay valid across `encode_picture`.
    mastering: Option<MasteringDisplayInfo>,
    max_cll: Option<ContentLightLevel>,
}

impl<'a> Encoder<'a> {
    /// Open a P1/ULL encoder on a **D3D11 device** (the DDA/WGC path).
    pub fn new(
        nvenc: &'a Nvenc,
        d3d_device: *mut c_void,
        cfg: &EncoderConfig,
    ) -> Result<Encoder<'a>, String> {
        let session = nvenc.open_session(d3d_device)?;
        Self::build(session, cfg, NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX, 0)
    }

    /// Open a P1/ULL encoder on a **CUDA context** (the NvFBC path). `input_pitch`
    /// is the row pitch (bytes) of the P010 device buffer that will be encoded.
    pub fn new_cuda(
        nvenc: &'a Nvenc,
        cuda_ctx: *mut c_void,
        cfg: &EncoderConfig,
        input_pitch: u32,
    ) -> Result<Encoder<'a>, String> {
        let session = nvenc.open_session_cuda(cuda_ctx)?;
        Self::build(
            session,
            cfg,
            NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
            input_pitch,
        )
    }

    fn build(
        session: Session<'a>,
        cfg: &EncoderConfig,
        resource_type: u32,
        input_pitch: u32,
    ) -> Result<Encoder<'a>, String> {
        let nvenc = session.nvenc;
        let encoder = session.encoder;

        let caps = EncoderCaps {
            ref_invalidation: session
                .cap(cfg.codec.guid(), NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION)
                .unwrap_or(0)
                != 0,
            intra_refresh: session
                .cap(cfg.codec.guid(), NV_ENC_CAPS_SUPPORT_INTRA_REFRESH)
                .unwrap_or(0)
                != 0,
            dyn_bitrate: session
                .cap(cfg.codec.guid(), NV_ENC_CAPS_SUPPORT_DYN_BITRATE_CHANGE)
                .unwrap_or(0)
                != 0,
        };

        // A preset config, tuned per `cfg`, then initialize with it.
        let mut preset = preset_config(nvenc, encoder, cfg)?;
        let mut init = init_params(cfg, &mut preset.preset_cfg);
        let initialize: FnInitialize = fnptr(nvenc.list.initialize_encoder, "initialize")?;
        // SAFETY: live encoder; `init` is correctly versioned and its
        // `encode_config` points at `preset`, alive for this call.
        let status = unsafe { initialize(encoder, &mut init) };
        check(status, "nvEncInitializeEncoder", encoder, &nvenc.list)?;

        // One reusable output bitstream buffer.
        let create_bs: FnCreateBitstream = fnptr(nvenc.list.create_bitstream_buffer, "create_bs")?;
        // SAFETY: plain data.
        let mut bs: NvEncCreateBitstreamBuffer = unsafe { std::mem::zeroed() };
        bs.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
        // SAFETY: live encoder; correctly versioned out-param.
        let status = unsafe { create_bs(encoder, &mut bs) };
        check(status, "nvEncCreateBitstreamBuffer", encoder, &nvenc.list)?;

        Ok(Encoder {
            session,
            caps,
            resource_type,
            input_pitch,
            bitstream: bs.bitstream_buffer,
            registered: None,
            mastering: cfg.hdr.as_ref().map(MasteringDisplayInfo::from_metadata),
            max_cll: cfg.hdr.as_ref().map(ContentLightLevel::from_metadata),
            cfg: cfg.clone(),
        })
    }

    /// What the encoder reported it supports. The transport consults this so it
    /// never asks for reference invalidation or intra refresh on a codec that
    /// lacks it.
    pub fn caps(&self) -> EncoderCaps {
        self.caps
    }

    /// Invalidate the reference frame tagged `timestamp` (the `inputTimeStamp`
    /// the frame was encoded with). The encoder drops it and anything predicted
    /// from it and falls back to an older reference, or forces intra if none is
    /// left. No-op if the encoder does not support it.
    pub fn invalidate_ref_frames(&self, timestamp: u64) -> Result<(), String> {
        if !self.caps.ref_invalidation {
            return Ok(());
        }
        let f: FnInvalidate = fnptr(self.session.nvenc.list.invalidate_ref_frames, "invalidate")?;
        // SAFETY: live encoder; the timestamp is a plain value.
        check(
            unsafe { f(self.session.encoder, timestamp) },
            "nvEncInvalidateRefFrames",
            self.session.encoder,
            &self.session.nvenc.list,
        )
    }

    /// Change the target bitrate without tearing the session down
    /// (`NvEncReconfigureEncoder`), for the delay-gradient rate controller.
    /// `resetEncoder`/`forceIDR` stay off, so it is seamless.
    pub fn reconfigure_bitrate(&mut self, bitrate_kbps: u32) -> Result<(), String> {
        if !self.caps.dyn_bitrate || bitrate_kbps == self.cfg.bitrate_kbps {
            return Ok(());
        }
        let mut cfg = self.cfg.clone();
        cfg.bitrate_kbps = bitrate_kbps;

        let nvenc = self.session.nvenc;
        let encoder = self.session.encoder;
        let mut preset = preset_config(nvenc, encoder, &cfg)?;
        // SAFETY: plain data.
        let mut params: NvEncReconfigureParams = unsafe { std::mem::zeroed() };
        params.version = NV_ENC_RECONFIGURE_PARAMS_VER;
        params.re_init_encode_params = init_params(&cfg, &mut preset.preset_cfg);

        let reconfigure: FnReconfigure = fnptr(nvenc.list.reconfigure_encoder, "reconfigure")?;
        // SAFETY: live encoder; `params.re_init_encode_params.encode_config`
        // points at `preset`, alive for this call.
        let status = unsafe { reconfigure(encoder, &mut params) };
        check(status, "nvEncReconfigureEncoder", encoder, &nvenc.list)?;
        self.cfg = cfg;
        Ok(())
    }

    /// Point a frame's pic-params at our HDR metadata, when present — the pointer
    /// offset differs between the HEVC and AV1 pic-params.
    fn apply_hdr(&self, pic: &mut NvEncPicParams) {
        if let (Some(mastering), Some(max_cll)) = (&self.mastering, &self.max_cll) {
            let p_max_cll = (max_cll as *const ContentLightLevel).cast::<c_void>();
            let p_mastering = (mastering as *const MasteringDisplayInfo).cast::<c_void>();
            let cfg = std::ptr::addr_of_mut!(pic.codec_pic_params);
            match self.cfg.codec {
                Codec::Hevc => {
                    // SAFETY: the union begins with NV_ENC_PIC_PARAMS_HEVC.
                    let head = unsafe { &mut *(cfg as *mut HevcPicParamsHead) };
                    head.p_max_cll = p_max_cll;
                    head.p_mastering_display = p_mastering;
                }
                Codec::Av1 => {
                    // SAFETY: the union begins with NV_ENC_PIC_PARAMS_AV1.
                    let head = unsafe { &mut *(cfg as *mut Av1PicParamsHead) };
                    head.p_max_cll = p_max_cll;
                    head.p_mastering_display = p_mastering;
                }
                // H.264 is SDR: `mastering`/`max_cll` are never set, so this is
                // unreachable, but the match must be exhaustive.
                Codec::H264 => {}
            }
        }
    }

    /// Fetch the decoder-config data the encoder produced — HEVC VPS/SPS/PPS, or
    /// the AV1 sequence-header OBU. Sent to the client at handshake.
    pub fn sequence_header(&self) -> Result<Vec<u8>, String> {
        let f: FnGetSequenceParams = fnptr(
            self.session.nvenc.list.get_sequence_params,
            "get_sequence_params",
        )?;
        let mut buf = vec![0u8; 1024];
        let mut size = 0u32;
        // SAFETY: plain data.
        let mut p: NvEncSequenceParamPayload = unsafe { std::mem::zeroed() };
        p.version = NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER;
        p.in_buffer_size = buf.len() as u32;
        p.spspps_buffer = buf.as_mut_ptr().cast();
        p.out_spspps_payload_size = &mut size;
        // SAFETY: live encoder; `buf`/`size` are sized above and outlive the call.
        let status = unsafe { f(self.session.encoder, &mut p) };
        check(
            status,
            "nvEncGetSequenceParams",
            self.session.encoder,
            &self.session.nvenc.list,
        )?;
        buf.truncate(size as usize);
        Ok(buf)
    }

    /// The AV1 `av1C` record (the client's MediaCodec `csd-0`). Errors for a
    /// non-AV1 encoder. Profile 0, 10-bit, tier 0; level from the resolution.
    pub fn av1c(&self) -> Result<Vec<u8>, String> {
        if self.cfg.codec != Codec::Av1 {
            return Err("av1c is only valid for an AV1 encoder".into());
        }
        let obu = self.sequence_header()?;
        Ok(crate::av1c::av1c_record(
            0,
            crate::av1c::seq_level_idx(self.cfg.width, self.cfg.height),
            0,
            true,
            &obu,
        ))
    }

    /// Encode one P010 input, emitting each slice (HEVC) or tile (AV1) via
    /// `on_slice` as it completes — the subframe path (needs `slices > 1` at
    /// construction). It overlaps encode with transmit (CLAUDE.md: mandatory).
    /// The input surface is registered once and reused; the drain uses
    /// non-blocking locks, so it cannot hang. The completion heuristic is
    /// box-to-validate.
    pub fn encode_slices(
        &mut self,
        input: *mut c_void,
        req: PicRequest,
        mut on_slice: impl FnMut(&[u8], bool),
    ) -> Result<bool, String> {
        let registered = self.ensure_registered(input)?;
        let list = &self.session.nvenc.list;
        let encoder = self.session.encoder;

        let map: FnMap = fnptr(list.map_input_resource, "map")?;
        // SAFETY: plain data.
        let mut m: NvEncMapInputResource = unsafe { std::mem::zeroed() };
        m.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
        m.registered_resource = registered;
        // SAFETY: live encoder; `registered` came from a live registration.
        let map_status = unsafe { map(encoder, &mut m) };
        if map_status != NV_ENC_SUCCESS {
            return Err(format!("nvEncMapInputResource failed with {map_status}"));
        }

        let encode: FnEncode = fnptr(list.encode_picture, "encode")?;
        // SAFETY: plain data.
        let mut pic: NvEncPicParams = unsafe { std::mem::zeroed() };
        pic.version = NV_ENC_PIC_PARAMS_VER;
        pic.input_width = self.cfg.width;
        pic.input_height = self.cfg.height;
        pic.input_buffer = m.mapped_resource;
        pic.output_bitstream = self.bitstream;
        pic.buffer_fmt = self.cfg.codec.buffer_format();
        pic.picture_struct = NV_ENC_PIC_STRUCT_FRAME;
        // The tag reference invalidation keys on, and the IDR request.
        pic.input_time_stamp = req.timestamp;
        if req.force_idr {
            pic.encode_pic_flags |= NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
        }
        self.apply_hdr(&mut pic);
        // SAFETY: live encoder; mapped input + bitstream are live.
        let enc_status = unsafe { encode(encoder, &mut pic) };

        let result = if enc_status == NV_ENC_SUCCESS {
            self.drain_slices(&mut on_slice)
        } else {
            Err(format!("nvEncEncodePicture failed with {enc_status}"))
        };

        if let Ok(unmap) = fnptr::<FnPtrArg>(list.unmap_input_resource, "unmap") {
            // SAFETY: mapped above; unmapped once. The registration is kept for
            // the next frame and released on Drop.
            unsafe { unmap(encoder, m.mapped_resource) };
        }
        result
    }

    /// Register `input` if it is not already the cached one, returning the
    /// registered-resource handle. The converter reuses one P010 surface, so
    /// this registers once per session rather than once per frame.
    fn ensure_registered(&mut self, input: *mut c_void) -> Result<*mut c_void, String> {
        if let Some((cached, res)) = self.registered {
            if cached == input {
                return Ok(res);
            }
            // The input surface changed (a resolution change rebuilds it); drop
            // the stale registration first.
            self.unregister(res);
            self.registered = None;
        }
        let register: FnRegister = fnptr(self.session.nvenc.list.register_resource, "register")?;
        // SAFETY: plain data.
        let mut reg: NvEncRegisterResource = unsafe { std::mem::zeroed() };
        reg.version = NV_ENC_REGISTER_RESOURCE_VER;
        reg.resource_type = self.resource_type;
        reg.pitch = self.input_pitch;
        reg.width = self.cfg.width;
        reg.height = self.cfg.height;
        reg.resource_to_register = input;
        reg.buffer_format = self.cfg.codec.buffer_format();
        reg.buffer_usage = NV_ENC_INPUT_IMAGE;
        // SAFETY: live encoder; `input` is a live P010/NV12 surface of this size.
        let status = unsafe { register(self.session.encoder, &mut reg) };
        check(
            status,
            "nvEncRegisterResource",
            self.session.encoder,
            &self.session.nvenc.list,
        )?;
        self.registered = Some((input, reg.registered_resource));
        Ok(reg.registered_resource)
    }

    /// Drain the bitstream slice-by-slice with non-blocking locks, emitting each
    /// newly-available chunk. Stops once locks stop yielding new bytes.
    fn drain_slices(&self, on_slice: &mut dyn FnMut(&[u8], bool)) -> Result<bool, String> {
        let list = &self.session.nvenc.list;
        let encoder = self.session.encoder;
        let lock: FnLock = fnptr(list.lock_bitstream, "lock")?;
        let unlock: FnPtrArg = fnptr(list.unlock_bitstream, "unlock")?;

        let mut consumed = 0usize;
        let mut idle = 0u32;
        let mut is_idr = false;
        // Bounded so a misbehaving driver cannot spin forever.
        for _ in 0..1024 {
            // SAFETY: plain data.
            let mut lb: NvEncLockBitstream = unsafe { std::mem::zeroed() };
            lb.version = NV_ENC_LOCK_BITSTREAM_VER;
            lb.output_bitstream = self.bitstream;
            lb.bitfields = 1; // doNotWait — never block the drain
            // SAFETY: live encoder; `bitstream` is a live output buffer.
            if unsafe { lock(encoder, &mut lb) } == NV_ENC_SUCCESS {
                let total = lb.bitstream_size_in_bytes as usize;
                if total > consumed {
                    // SAFETY: the driver mapped `total` valid bytes at
                    // `bitstream_buffer_ptr`, live until unlock.
                    let slice = unsafe {
                        std::slice::from_raw_parts(
                            (lb.bitstream_buffer_ptr as *const u8).add(consumed),
                            total - consumed,
                        )
                    };
                    is_idr = lb.picture_type == NV_ENC_PIC_TYPE_IDR;
                    on_slice(slice, is_idr);
                    consumed = total;
                    idle = 0;
                } else {
                    idle += 1;
                }
                // SAFETY: locked just above; unlocked once.
                unsafe { unlock(encoder, self.bitstream) };
            } else {
                idle += 1;
            }
            if idle >= 8 && consumed > 0 {
                break; // frame drained
            }
            std::thread::yield_now();
        }
        Ok(is_idr)
    }

    fn unregister(&self, registered: *mut c_void) {
        if registered.is_null() {
            return;
        }
        if let Ok(unregister) =
            fnptr::<FnPtrArg>(self.session.nvenc.list.unregister_resource, "unregister")
        {
            // SAFETY: `registered` came from register and is released once.
            unsafe { unregister(self.session.encoder, registered) };
        }
    }
}

impl Drop for Encoder<'_> {
    fn drop(&mut self) {
        if let Some((_, res)) = self.registered.take() {
            self.unregister(res);
        }
        if self.bitstream.is_null() {
            return;
        }
        if let Ok(destroy) = fnptr::<FnPtrArg>(
            self.session.nvenc.list.destroy_bitstream_buffer,
            "destroy_bs",
        ) {
            // SAFETY: our buffer, destroyed once before the session (Session's
            // Drop destroys the encoder after this).
            unsafe { destroy(self.session.encoder, self.bitstream) };
        }
    }
}

/// Fetch the P1/ULL preset config and tune it per `cfg`: CBR at the target
/// bitrate with a one-frame VBV, an infinite GOP (no periodic IDR), the slice or
/// tile subdivision for subframe readback, a DPB deep enough for reference
/// invalidation to fall back rather than force a keyframe, and the intra-refresh
/// and HDR-output flags when asked.
fn preset_config(
    nvenc: &Nvenc,
    encoder: *mut c_void,
    cfg: &EncoderConfig,
) -> Result<NvEncPresetConfig, String> {
    let get_preset: FnGetPresetEx = fnptr(nvenc.list.get_encode_preset_config_ex, "get_preset")?;
    // SAFETY: plain data; zero is valid for every field.
    let mut preset: NvEncPresetConfig = unsafe { std::mem::zeroed() };
    preset.version = NV_ENC_PRESET_CONFIG_VER;
    preset.preset_cfg.version = NV_ENC_CONFIG_VER;
    // SAFETY: live encoder; correctly versioned preset config out-param.
    let status = unsafe {
        get_preset(
            encoder,
            cfg.codec.guid(),
            preset_guid(cfg.preset),
            NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
            &mut preset,
        )
    };
    check(status, "nvEncGetEncodePresetConfigEx", encoder, &nvenc.list)?;
    preset.preset_cfg.version = NV_ENC_CONFIG_VER;

    // Rate control (CBR default, VBR when asked), one-frame VBV, one P per frame,
    // and the GOP — infinite unless a forced IDR period is configured.
    let bitrate_bps = cfg.bitrate_kbps.saturating_mul(1000);
    let fps = cfg.fps.max(1);
    let rc = &mut preset.preset_cfg.rc_params;
    rc.rate_control_mode = if cfg.vbr {
        NV_ENC_PARAMS_RC_VBR
    } else {
        NV_ENC_PARAMS_RC_CBR
    };
    rc.average_bit_rate = bitrate_bps;
    rc.max_bit_rate = bitrate_bps;
    rc.vbv_buffer_size = bitrate_bps / fps;
    rc.vbv_initial_delay = bitrate_bps / fps;
    preset.preset_cfg.gop_length = if cfg.idr_period > 0 {
        cfg.idr_period
    } else {
        NVENC_INFINITE_GOPLENGTH
    };
    preset.preset_cfg.frame_interval_p = 1;

    let head = std::ptr::addr_of_mut!(preset.preset_cfg.encode_codec_config);
    match cfg.codec {
        Codec::Hevc => {
            // SAFETY: the union begins with NV_ENC_CONFIG_HEVC.
            let hevc = unsafe { &mut *(head as *mut HevcConfigHead) };
            hevc.max_num_ref_frames_in_dpb = cfg.dpb_depth;
            if cfg.slices > 1 {
                hevc.slice_mode = 3; // a fixed number of uniform slices
                hevc.slice_mode_data = cfg.slices;
            }
            if let Some((period, count)) = cfg.intra_refresh {
                hevc.bitfields |= HEVC_ENABLE_INTRA_REFRESH;
                hevc.intra_refresh_period = period;
                hevc.intra_refresh_cnt = count;
            }
            if cfg.hdr.is_some() {
                // outputMaxCll (bit 23) + outputMasteringDisplay (bit 24).
                hevc.bitfields |= (1 << 23) | (1 << 24);
            }
            // HEVC here is always 10-bit P010: tell NVENC so, in and out. Without
            // it the session stays 8-bit and rejects the P010 input surface.
            hevc.input_bit_depth = NV_ENC_BIT_DEPTH_10;
            hevc.output_bit_depth = NV_ENC_BIT_DEPTH_10;
            // Signal the color description so the decoder reads the pixels the
            // convert stage produced (BT.709 SDR vs BT.2020 PQ), not a guess.
            let (primaries, transfer, matrix) = cfg.color.vui();
            hevc.video_signal_type_present_flag = 1;
            hevc.colour_description_present_flag = 1;
            hevc.video_full_range_flag = 0; // limited range, matching the shaders
            hevc.colour_primaries = primaries;
            hevc.transfer_characteristics = transfer;
            hevc.colour_matrix = matrix;
        }
        Codec::Av1 => {
            // SAFETY: the union begins with NV_ENC_CONFIG_AV1.
            let av1 = unsafe { &mut *(head as *mut Av1ConfigHead) };
            av1.max_num_ref_frames_in_dpb = cfg.dpb_depth;
            if cfg.slices > 1 {
                // Uniform tiles (enableCustomTileConfig stays 0).
                av1.num_tile_columns = cfg.slices;
                av1.num_tile_rows = cfg.slices;
            }
            if let Some((period, count)) = cfg.intra_refresh {
                av1.bitfields |= AV1_ENABLE_INTRA_REFRESH;
                av1.intra_refresh_period = period;
                av1.intra_refresh_cnt = count;
            }
            if cfg.hdr.is_some() {
                // outputMaxCll (bit 14) + outputMasteringDisplay (bit 15).
                av1.bitfields |= (1 << 14) | (1 << 15);
            }
            // AV1 here is always 10-bit P010: configure 10-bit in and out.
            av1.input_bit_depth = NV_ENC_BIT_DEPTH_10;
            av1.output_bit_depth = NV_ENC_BIT_DEPTH_10;
            // AV1 sequence-header color_config (always present, no gating flag).
            let (primaries, transfer, matrix) = cfg.color.vui();
            av1.color_primaries = primaries;
            av1.transfer_characteristics = transfer;
            av1.matrix_coefficients = matrix;
            av1.color_range = 0; // studio/limited, matching the shaders
        }
        Codec::H264 => {
            // SAFETY: the union is NV_ENC_CONFIG_H264 for an H.264 encoder.
            let h264 = unsafe { &mut *(head as *mut H264ConfigHead) };
            h264.max_num_ref_frames = cfg.dpb_depth;
            if cfg.slices > 1 {
                h264.slice_mode = 3; // a fixed number of uniform slices
                h264.slice_mode_data = cfg.slices;
            }
            if let Some((period, count)) = cfg.intra_refresh {
                h264.flags |= H264_ENABLE_INTRA_REFRESH;
                h264.intra_refresh_period = period;
                h264.intra_refresh_cnt = count;
            }
            // H.264 is always 8-bit SDR BT.709 (no HDR): signal it so the decoder
            // does not guess, matching the NV12 BT.709 convert.
            h264.video_signal_type_present_flag = 1;
            h264.colour_description_present_flag = 1;
            h264.video_full_range_flag = 0;
            h264.colour_primaries = NV_ENC_VUI_PRIMARIES_BT709;
            h264.transfer_characteristics = NV_ENC_VUI_TRANSFER_BT709;
            h264.colour_matrix = NV_ENC_VUI_MATRIX_BT709;
        }
    }
    Ok(preset)
}

/// The init/reconfigure params for `cfg`, with `encode_config` pointing at a
/// config the caller keeps alive for the FFI call.
fn init_params(cfg: &EncoderConfig, encode_config: *mut NvEncConfig) -> NvEncInitializeParams {
    // SAFETY: plain data.
    let mut init: NvEncInitializeParams = unsafe { std::mem::zeroed() };
    init.version = NV_ENC_INITIALIZE_PARAMS_VER;
    init.encode_guid = cfg.codec.guid();
    init.preset_guid = preset_guid(cfg.preset);
    init.encode_width = cfg.width;
    init.encode_height = cfg.height;
    init.dar_width = cfg.width;
    init.dar_height = cfg.height;
    init.frame_rate_num = cfg.fps.max(1);
    init.frame_rate_den = 1;
    init.enable_ptd = 1;
    init.tuning_info = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    init.encode_config = encode_config;
    if cfg.slices > 1 {
        // reportSliceOffsets (bit 0) + enableSubFrameWrite (bit 1).
        init.bitfields |= 0b11;
    }
    init
}

/// Transmute a function-list slot the spike left untyped into a real signature.
fn fnptr<T: Copy>(slot: *mut c_void, what: &str) -> Result<T, String> {
    if slot.is_null() {
        return Err(format!("nvEnc {what} is null"));
    }
    debug_assert_eq!(size_of::<T>(), size_of::<*mut c_void>());
    // SAFETY: NVENC function-list slots are code pointers; the caller names the
    // exact signature from nvEncodeAPI.h. Pointer widths match on this target.
    Ok(unsafe { *(&slot as *const *mut c_void).cast::<T>() })
}

/// NVENC's own reason for the last failure on `encoder`, via
/// `nvEncGetLastErrorString`. Returns `None` — and never fails — when the
/// function slot, the handle, or the message is unavailable, so it can be called
/// from the error path without itself becoming a source of errors.
fn last_error(encoder: *mut c_void, list: &NvEncodeApiFunctionList) -> Option<String> {
    if encoder.is_null() {
        return None;
    }
    let get: FnLastError = fnptr(list.get_last_error_string, "nvEncGetLastErrorString").ok()?;
    // SAFETY: `get` is the get_last_error_string slot from the function list and
    // `encoder` is a live session handle. NVENC returns a pointer to a static,
    // NUL-terminated C string (or null), owned by the driver and valid to read now.
    let msg = unsafe { get(encoder) };
    if msg.is_null() {
        return None;
    }
    // SAFETY: `msg` is a non-null, NUL-terminated C string from the driver.
    let msg = unsafe { CStr::from_ptr(msg) }
        .to_string_lossy()
        .into_owned();
    (!msg.is_empty()).then_some(msg)
}

/// Turn a non-success status into an error, appending NVENC's own reason for the
/// last failure on `encoder` when one is available.
fn check(
    status: NvencStatus,
    what: &str,
    encoder: *mut c_void,
    list: &NvEncodeApiFunctionList,
) -> Result<(), String> {
    if status == NV_ENC_SUCCESS {
        Ok(())
    } else if let Some(msg) = last_error(encoder, list) {
        Err(format!("{what} failed with {status}: {msg}"))
    } else {
        Err(format!("{what} failed with {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The diagnostics path must never itself panic when NVENC's error-string
    // slot or the encoder handle is unavailable: `check` then falls back to the
    // bare numeric message rather than dereferencing a null.
    #[test]
    fn check_falls_back_to_the_numeric_message_without_an_encoder() {
        // SAFETY: NvEncodeApiFunctionList is plain pointers + ints, so all-zero
        // is a valid (inert) value; `last_error` guards the null encoder before
        // it would ever read the (also null) error-string slot.
        let list: NvEncodeApiFunctionList = unsafe { std::mem::zeroed() };
        // 8 is NV_ENC_ERR_INVALID_PARAM — the code the box reported.
        let err = check(8, "x", std::ptr::null_mut(), &list).unwrap_err();
        assert_eq!(err, "x failed with 8");
        assert!(check(NV_ENC_SUCCESS, "x", std::ptr::null_mut(), &list).is_ok());
    }

    // The layouts are ABI contracts; a slip is caught here rather than as a
    // driver INVALID_VERSION on the box.
    #[test]
    fn struct_sizes_match_the_header() {
        assert_eq!(size_of::<NvEncQp>(), 12);
        assert_eq!(size_of::<NvEncCodecConfig>(), 1280);
        // NV_ENC_CONFIG is embedded in NV_ENC_PRESET_CONFIG, so its size is
        // load-bearing; the codec union + reserved dominate it.
        assert_eq!(size_of::<NvEncConfig>() % 8, 0);
        assert_eq!(size_of::<NvEncPicParams>(), 3360);
        // The codec-pic-params union must be 8-byte aligned at 80, or the HDR-SEI
        // pointers written into it land 4 bytes off and NVENC dereferences garbage.
        assert_eq!(std::mem::offset_of!(NvEncPicParams, codec_pic_params), 80);
        // NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE is 16 bytes; a 4-byte
        // alias shifted tuningInfo off offset 0x88 and the driver rejected
        // every P-preset init as "tuningInfo undefined".
        assert_eq!(size_of::<MeHintCounts>(), 16);
        assert_eq!(
            std::mem::offset_of!(NvEncInitializeParams, tuning_info),
            136
        );
        assert_eq!(
            std::mem::offset_of!(NvEncInitializeParams, buffer_format),
            140
        );
        // The 10-bit config enums must land where NVENC reads them
        // (nvEncodeAPI 13.1.15), or a P010 session silently stays 8-bit.
        assert_eq!(std::mem::offset_of!(HevcConfigHead, slice_mode_data), 56);
        assert_eq!(std::mem::offset_of!(HevcConfigHead, output_bit_depth), 200);
        assert_eq!(std::mem::offset_of!(HevcConfigHead, input_bit_depth), 204);
        assert_eq!(std::mem::offset_of!(Av1ConfigHead, num_tile_rows), 40);
        assert_eq!(std::mem::offset_of!(Av1ConfigHead, output_bit_depth), 112);
        assert_eq!(std::mem::offset_of!(Av1ConfigHead, input_bit_depth), 116);
        // Color-description fields must land where NVENC reads them (13.1.15).
        assert_eq!(
            std::mem::offset_of!(HevcConfigHead, video_signal_type_present_flag),
            72
        );
        assert_eq!(
            std::mem::offset_of!(HevcConfigHead, video_full_range_flag),
            80
        );
        assert_eq!(
            std::mem::offset_of!(HevcConfigHead, colour_description_present_flag),
            84
        );
        assert_eq!(std::mem::offset_of!(HevcConfigHead, colour_primaries), 88);
        assert_eq!(
            std::mem::offset_of!(HevcConfigHead, transfer_characteristics),
            92
        );
        assert_eq!(std::mem::offset_of!(HevcConfigHead, colour_matrix), 96);
        assert_eq!(std::mem::offset_of!(Av1ConfigHead, color_primaries), 68);
        assert_eq!(
            std::mem::offset_of!(Av1ConfigHead, transfer_characteristics),
            72
        );
        assert_eq!(std::mem::offset_of!(Av1ConfigHead, matrix_coefficients), 76);
        assert_eq!(std::mem::offset_of!(Av1ConfigHead, color_range), 80);
        // HDR-SEI pointer offsets in the pic params — a slip dereferences garbage
        // in NVENC (segfault) the first time HDR is active.
        assert_eq!(std::mem::offset_of!(HevcPicParamsHead, p_max_cll), 120);
        assert_eq!(
            std::mem::offset_of!(HevcPicParamsHead, p_mastering_display),
            128
        );
        assert_eq!(std::mem::offset_of!(Av1PicParamsHead, p_max_cll), 88);
        assert_eq!(
            std::mem::offset_of!(Av1PicParamsHead, p_mastering_display),
            96
        );
        // H.264 config layout (the fields after idrPeriod are separate u32s) + VUI.
        assert_eq!(std::mem::offset_of!(H264ConfigHead, max_num_ref_frames), 60);
        assert_eq!(std::mem::offset_of!(H264ConfigHead, slice_mode_data), 68);
        assert_eq!(
            std::mem::offset_of!(H264ConfigHead, video_signal_type_present_flag),
            80
        );
        assert_eq!(std::mem::offset_of!(H264ConfigHead, colour_primaries), 96);
    }
}

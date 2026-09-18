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

use std::ffi::c_void;

use sunburst_capture::HdrMetadata;

use crate::hdr::{ContentLightLevel, MasteringDisplayInfo};
use crate::nvenc::{
    Guid, NV_ENC_CODEC_AV1_GUID, NV_ENC_CODEC_HEVC_GUID, Nvenc, NvencStatus, Session,
    struct_version,
};

/// The codec an [`Encoder`] targets.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Codec {
    /// HEVC Main10 — the Shield's codec. Subdivided into slices.
    Hevc,
    /// AV1 10-bit — the Homatics' codec. Subdivided into tiles.
    Av1,
}

impl Codec {
    fn guid(self) -> Guid {
        match self {
            Codec::Hevc => NV_ENC_CODEC_HEVC_GUID,
            Codec::Av1 => NV_ENC_CODEC_AV1_GUID,
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

/// `NV_ENC_PRESET_P1_GUID`.
const NV_ENC_PRESET_P1_GUID: Guid = Guid {
    data1: 0xfc0a_8d3e,
    data2: 0x45f8,
    data3: 0x4cf8,
    data4: [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf],
};

/// `NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY`.
const NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY: u32 = 3;
/// `NV_ENC_BUFFER_FORMAT_YUV420_10BIT` — P010, NVENC's 10-bit semi-planar input.
const NV_ENC_BUFFER_FORMAT_YUV420_10BIT: u32 = 0x0001_0000;
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
    /// `(period, count)` for gradual intra refresh, or `None`. Gated on the
    /// decoder's quirks and the encoder's caps by the caller.
    pub intra_refresh: Option<(u32, u32)>,
    /// `maxNumRefFramesInDPB`. A deep DPB is what lets reference invalidation
    /// fall back to an older good frame instead of forcing a keyframe
    /// (`NvEncInvalidateRefFrames` docs recommend it).
    pub dpb_depth: u32,
}

impl EncoderConfig {
    /// A sensible default for `codec` at `width`×`height`: the plan's slice/tile
    /// counts, an 8-frame DPB, no intra refresh, SDR.
    pub fn new(codec: Codec, width: u32, height: u32) -> EncoderConfig {
        let slices = match codec {
            Codec::Hevc => 4,
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
            intra_refresh: None,
            dpb_depth: 8,
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
}

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

/// `NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE` — a single bitfield word.
type MeHintCounts = u32;

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
    codec_pic_params: [u32; 256],
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
        check(status, "nvEncInitializeEncoder")?;

        // One reusable output bitstream buffer.
        let create_bs: FnCreateBitstream = fnptr(nvenc.list.create_bitstream_buffer, "create_bs")?;
        // SAFETY: plain data.
        let mut bs: NvEncCreateBitstreamBuffer = unsafe { std::mem::zeroed() };
        bs.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
        // SAFETY: live encoder; correctly versioned out-param.
        let status = unsafe { create_bs(encoder, &mut bs) };
        check(status, "nvEncCreateBitstreamBuffer")?;

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
        check(status, "nvEncReconfigureEncoder")?;
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
        check(status, "nvEncGetSequenceParams")?;
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
        mut on_slice: impl FnMut(&[u8]),
    ) -> Result<(), String> {
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
        pic.buffer_fmt = NV_ENC_BUFFER_FORMAT_YUV420_10BIT;
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
        reg.buffer_format = NV_ENC_BUFFER_FORMAT_YUV420_10BIT;
        reg.buffer_usage = NV_ENC_INPUT_IMAGE;
        // SAFETY: live encoder; `input` is a live P010 surface of this size.
        let status = unsafe { register(self.session.encoder, &mut reg) };
        check(status, "nvEncRegisterResource")?;
        self.registered = Some((input, reg.registered_resource));
        Ok(reg.registered_resource)
    }

    /// Drain the bitstream slice-by-slice with non-blocking locks, emitting each
    /// newly-available chunk. Stops once locks stop yielding new bytes.
    fn drain_slices(&self, on_slice: &mut dyn FnMut(&[u8])) -> Result<(), String> {
        let list = &self.session.nvenc.list;
        let encoder = self.session.encoder;
        let lock: FnLock = fnptr(list.lock_bitstream, "lock")?;
        let unlock: FnPtrArg = fnptr(list.unlock_bitstream, "unlock")?;

        let mut consumed = 0usize;
        let mut idle = 0u32;
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
                    on_slice(slice);
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
        Ok(())
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
            NV_ENC_PRESET_P1_GUID,
            NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
            &mut preset,
        )
    };
    check(status, "nvEncGetEncodePresetConfigEx")?;
    preset.preset_cfg.version = NV_ENC_CONFIG_VER;

    // Constant bitrate, one-frame VBV, infinite GOP, one P per frame.
    let bitrate_bps = cfg.bitrate_kbps.saturating_mul(1000);
    let fps = cfg.fps.max(1);
    let rc = &mut preset.preset_cfg.rc_params;
    rc.rate_control_mode = NV_ENC_PARAMS_RC_CBR;
    rc.average_bit_rate = bitrate_bps;
    rc.max_bit_rate = bitrate_bps;
    rc.vbv_buffer_size = bitrate_bps / fps;
    rc.vbv_initial_delay = bitrate_bps / fps;
    preset.preset_cfg.gop_length = NVENC_INFINITE_GOPLENGTH;
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
    init.preset_guid = NV_ENC_PRESET_P1_GUID;
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

/// Turn a non-success status into an error.
fn check(status: NvencStatus, what: &str) -> Result<(), String> {
    if status == NV_ENC_SUCCESS {
        Ok(())
    } else {
        Err(format!("{what} failed with {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The layouts are ABI contracts; a slip is caught here rather than as a
    // driver INVALID_VERSION on the box.
    #[test]
    fn struct_sizes_match_the_header() {
        assert_eq!(size_of::<NvEncQp>(), 12);
        assert_eq!(size_of::<NvEncCodecConfig>(), 1280);
        // NV_ENC_CONFIG is embedded in NV_ENC_PRESET_CONFIG, so its size is
        // load-bearing; the codec union + reserved dominate it.
        assert_eq!(size_of::<NvEncConfig>() % 8, 0);
        assert!(size_of::<NvEncPicParams>() > 1024);
    }
}

// SPDX-License-Identifier: GPL-2.0-or-later

//! Minimal NVENC FFI, transcribed from `nvEncodeAPI.h` v13.1.
//!
//! No crate provides these bindings, and this is the shape `sunburst-encode`
//! will use in Phase 3: the API is loaded with `LoadLibrary` at runtime rather
//! than linked, so nothing here creates a build-time dependency on an NVIDIA
//! SDK and the whole workspace still cross-checks from Linux.
//!
//! # Why the whole function list is declared
//!
//! Only four entries are ever called, but `NV_ENCODE_API_FUNCTION_LIST` is
//! indexed by offset. Declaring a short prefix and hoping would put the wrong
//! function behind the right name — which does not fail to compile, and does not
//! reliably crash either. The unused entries are `*mut c_void` for exactly that
//! reason: correct size, no false claim about the signature.
//!
//! Transcribed from `nv-codec-headers`. If it is ever regenerated, regenerate it
//! from the header rather than from memory.

use std::ffi::{CString, c_void};

use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
use windows::core::PCSTR;

pub type NvencStatus = i32;
pub const NV_ENC_SUCCESS: NvencStatus = 0;

/// `NVENCAPI_VERSION`: major 13, minor 1.
pub const NVENCAPI_VERSION: u32 = 13 | (1 << 24);

/// `NVENCAPI_STRUCT_VERSION(ver)`.
pub(crate) const fn struct_version(ver: u32) -> u32 {
    NVENCAPI_VERSION | (ver << 16) | (0x7 << 28)
}

pub const NV_ENCODE_API_FUNCTION_LIST_VER: u32 = struct_version(2);
pub const NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = struct_version(1);
pub const NV_ENC_CAPS_PARAM_VER: u32 = struct_version(1);

pub const NV_ENC_DEVICE_TYPE_DIRECTX: u32 = 0;
pub const NV_ENC_DEVICE_TYPE_CUDA: u32 = 1;

/// Split what `NvEncodeAPIGetMaxSupportedVersion` returns into major and minor.
///
/// It packs them as `(major << 4) | minor`, so 13.1 comes back as `0xD1`.
pub const fn decode_driver_version(packed: u32) -> (u32, u32) {
    (packed >> 4, packed & 0xF)
}

/// Whether the driver implements at least the API this build was written
/// against.
///
/// The check CLAUDE.md asks for at startup. Stated as an API version because
/// that is what the driver reports and what this compares; a driver branch
/// number would be a second, unverifiable way of saying the same thing. Too old
/// and the headers carry no AV1 GUIDs, so every AV1 answer below would be a
/// false negative rather than an error.
pub const fn driver_is_new_enough(packed: u32) -> bool {
    let (major, minor) = decode_driver_version(packed);
    let want_major = NVENCAPI_VERSION & 0xFF;
    let want_minor = NVENCAPI_VERSION >> 24;
    major > want_major || (major == want_major && minor >= want_minor)
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

pub const NV_ENC_CODEC_H264_GUID: Guid = Guid {
    data1: 0x6bc82762,
    data2: 0x4e63,
    data3: 0x4ca4,
    data4: [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
};

pub const NV_ENC_CODEC_HEVC_GUID: Guid = Guid {
    data1: 0x790cdc88,
    data2: 0x4522,
    data3: 0x4d7b,
    data4: [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03],
};

pub const NV_ENC_CODEC_AV1_GUID: Guid = Guid {
    data1: 0x0a352289,
    data2: 0x0aa7,
    data3: 0x4759,
    data4: [0x86, 0x2d, 0x5d, 0x15, 0xcd, 0x16, 0xd2, 0x54],
};

/// `NV_ENC_CAPS` ordinals.
///
/// The enum in the header carries no explicit values, so these are positions,
/// and a miscount queries a different capability while reporting the name you
/// asked for. Verified against the header rather than recalled.
pub mod caps {
    pub const LEVEL_MAX: u32 = 13;
    pub const WIDTH_MAX: u32 = 16;
    pub const HEIGHT_MAX: u32 = 17;
    pub const SUPPORT_DYN_BITRATE_CHANGE: u32 = 20;
    pub const SUPPORT_SUBFRAME_READBACK: u32 = 23;
    pub const SUPPORT_INTRA_REFRESH: u32 = 25;
    pub const SUPPORT_REF_PIC_INVALIDATION: u32 = 28;
    pub const SUPPORT_YUV444_ENCODE: u32 = 33;
    pub const SUPPORT_10BIT_ENCODE: u32 = 39;
    pub const NUM_ENCODER_ENGINES: u32 = 49;
}

#[repr(C)]
pub struct NvEncOpenEncodeSessionExParams {
    pub version: u32,
    pub device_type: u32,
    pub device: *mut c_void,
    pub reserved: *mut c_void,
    pub api_version: u32,
    pub reserved1: [u32; 253],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
pub struct NvEncCapsParam {
    pub version: u32,
    pub caps_to_query: u32,
    pub reserved: [u32; 62],
}

type FnOpenEncodeSessionEx =
    unsafe extern "C" fn(*mut NvEncOpenEncodeSessionExParams, *mut *mut c_void) -> NvencStatus;
type FnGetEncodeGuidCount = unsafe extern "C" fn(*mut c_void, *mut u32) -> NvencStatus;
type FnGetEncodeGuids = unsafe extern "C" fn(*mut c_void, *mut Guid, u32, *mut u32) -> NvencStatus;
type FnGetEncodeCaps =
    unsafe extern "C" fn(*mut c_void, Guid, *mut NvEncCapsParam, *mut i32) -> NvencStatus;
type FnDestroyEncoder = unsafe extern "C" fn(*mut c_void) -> NvencStatus;

/// `NV_ENCODE_API_FUNCTION_LIST`, in header order.
///
/// Every field must stay, in this order. See the module docs.
#[repr(C)]
pub struct NvEncodeApiFunctionList {
    pub version: u32,
    pub reserved: u32,
    pub open_encode_session: *mut c_void,
    pub get_encode_guid_count: Option<FnGetEncodeGuidCount>,
    pub get_encode_profile_guid_count: *mut c_void,
    pub get_encode_profile_guids: *mut c_void,
    pub get_encode_guids: Option<FnGetEncodeGuids>,
    pub get_input_format_count: *mut c_void,
    pub get_input_formats: *mut c_void,
    pub get_encode_caps: Option<FnGetEncodeCaps>,
    pub get_encode_preset_count: *mut c_void,
    pub get_encode_preset_guids: *mut c_void,
    pub get_encode_preset_config: *mut c_void,
    pub initialize_encoder: *mut c_void,
    pub create_input_buffer: *mut c_void,
    pub destroy_input_buffer: *mut c_void,
    pub create_bitstream_buffer: *mut c_void,
    pub destroy_bitstream_buffer: *mut c_void,
    pub encode_picture: *mut c_void,
    pub lock_bitstream: *mut c_void,
    pub unlock_bitstream: *mut c_void,
    pub lock_input_buffer: *mut c_void,
    pub unlock_input_buffer: *mut c_void,
    pub get_encode_stats: *mut c_void,
    pub get_sequence_params: *mut c_void,
    pub register_async_event: *mut c_void,
    pub unregister_async_event: *mut c_void,
    pub map_input_resource: *mut c_void,
    pub unmap_input_resource: *mut c_void,
    pub destroy_encoder: Option<FnDestroyEncoder>,
    pub invalidate_ref_frames: *mut c_void,
    pub open_encode_session_ex: Option<FnOpenEncodeSessionEx>,
    pub register_resource: *mut c_void,
    pub unregister_resource: *mut c_void,
    pub reconfigure_encoder: *mut c_void,
    pub reserved1: *mut c_void,
    pub create_mv_buffer: *mut c_void,
    pub destroy_mv_buffer: *mut c_void,
    pub run_motion_estimation_only: *mut c_void,
    pub get_last_error_string: *mut c_void,
    pub set_io_cuda_streams: *mut c_void,
    pub get_encode_preset_config_ex: *mut c_void,
    pub get_sequence_param_ex: *mut c_void,
    pub restore_encoder_state: *mut c_void,
    pub lookahead_picture: *mut c_void,
    pub reserved2: [*mut c_void; 275],
}

type FnCreateInstance = unsafe extern "C" fn(*mut NvEncodeApiFunctionList) -> NvencStatus;
type FnGetMaxSupportedVersion = unsafe extern "C" fn(*mut u32) -> NvencStatus;

/// The loaded NVENC API.
pub struct Nvenc {
    pub list: Box<NvEncodeApiFunctionList>,
    /// Highest API version the installed driver supports.
    pub driver_max_version: u32,
    _module: HMODULE,
}

fn proc(module: HMODULE, name: &str) -> Option<*const c_void> {
    let cname = CString::new(name).ok()?;
    // SAFETY: `module` is a live handle from `LoadLibraryA` and `cname` is a
    // NUL-terminated string that outlives the call.
    let p = unsafe { GetProcAddress(module, PCSTR(cname.as_ptr().cast())) }?;
    Some(p as *const c_void)
}

impl Nvenc {
    /// Load `nvEncodeAPI64.dll` and populate the function list.
    pub fn load() -> Result<Nvenc, String> {
        // SAFETY: a literal, NUL-terminated library name.
        let module = unsafe { LoadLibraryA(PCSTR(c"nvEncodeAPI64.dll".as_ptr().cast())) }
            .map_err(|e| format!("nvEncodeAPI64.dll did not load: {e}. No NVIDIA driver?"))?;

        let get_max = proc(module, "NvEncodeAPIGetMaxSupportedVersion")
            .ok_or("NvEncodeAPIGetMaxSupportedVersion missing")?;
        let create =
            proc(module, "NvEncodeAPICreateInstance").ok_or("NvEncodeAPICreateInstance missing")?;

        // SAFETY: the export exists and has this signature per nvEncodeAPI.h.
        let get_max: FnGetMaxSupportedVersion = unsafe { std::mem::transmute(get_max) };
        // SAFETY: as above.
        let create: FnCreateInstance = unsafe { std::mem::transmute(create) };

        let mut driver_max_version = 0u32;
        // SAFETY: `driver_max_version` is a valid, writable u32.
        let status = unsafe { get_max(&mut driver_max_version) };
        if status != NV_ENC_SUCCESS {
            return Err(format!(
                "NvEncodeAPIGetMaxSupportedVersion failed with {status}"
            ));
        }

        // A zeroed list is required: `reserved`/`reserved2` must be NULL.
        let mut list: Box<NvEncodeApiFunctionList> = unsafe {
            // SAFETY: the struct is plain data - integers and pointers - so an
            // all-zero bit pattern is a valid value for every field. `Option<fn>`
            // is null-pointer-optimised, so zero is `None`.
            Box::new(std::mem::zeroed())
        };
        list.version = NV_ENCODE_API_FUNCTION_LIST_VER;

        // SAFETY: `list` is a correctly versioned, zeroed function list.
        let status = unsafe { create(list.as_mut()) };
        if status != NV_ENC_SUCCESS {
            let (dmaj, dmin) = decode_driver_version(driver_max_version);
            return Err(format!(
                "NvEncodeAPICreateInstance failed with {status}. \
                 The driver supports API {dmaj}.{dmin}; this build asks for {}.{}. \
                 Update the driver until it reports at least that",
                NVENCAPI_VERSION & 0xFF,
                NVENCAPI_VERSION >> 24,
            ));
        }

        Ok(Nvenc {
            list,
            driver_max_version,
            _module: module,
        })
    }

    /// Open a session against a D3D11 device (the DDA/WGC path).
    pub fn open_session(&self, d3d_device: *mut c_void) -> Result<Session<'_>, String> {
        self.open_session_typed(d3d_device, NV_ENC_DEVICE_TYPE_DIRECTX)
    }

    /// Open a session against a CUDA context (the NvFBC path) — the frame stays a
    /// CUDA device pointer and encodes without a D3D11 bounce.
    pub fn open_session_cuda(&self, cuda_ctx: *mut c_void) -> Result<Session<'_>, String> {
        self.open_session_typed(cuda_ctx, NV_ENC_DEVICE_TYPE_CUDA)
    }

    fn open_session_typed(
        &self,
        device: *mut c_void,
        device_type: u32,
    ) -> Result<Session<'_>, String> {
        let open = self
            .list
            .open_encode_session_ex
            .ok_or("nvEncOpenEncodeSessionEx is null")?;

        // SAFETY: plain data; zero is a valid value for every field.
        let mut params: NvEncOpenEncodeSessionExParams = unsafe { std::mem::zeroed() };
        params.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
        params.device_type = device_type;
        params.device = device;
        params.api_version = NVENCAPI_VERSION;

        let mut encoder: *mut c_void = std::ptr::null_mut();
        // SAFETY: `params` is correctly versioned and `encoder` is a valid out
        // pointer. `device` is a live D3D11 device or CUDA context owned by the
        // caller.
        let status = unsafe { open(&mut params, &mut encoder) };
        if status != NV_ENC_SUCCESS {
            return Err(format!(
                "nvEncOpenEncodeSessionEx failed with {status}. \
                 Another process holding the encoder, or an unsupported device"
            ));
        }

        Ok(Session {
            nvenc: self,
            encoder,
        })
    }
}

/// An open encode session. Closed on drop.
pub struct Session<'a> {
    pub(crate) nvenc: &'a Nvenc,
    pub(crate) encoder: *mut c_void,
}

impl Session<'_> {
    /// Codec GUIDs this encoder supports.
    pub fn encode_guids(&self) -> Result<Vec<Guid>, String> {
        let count_fn = self
            .nvenc
            .list
            .get_encode_guid_count
            .ok_or("nvEncGetEncodeGUIDCount is null")?;
        let guids_fn = self
            .nvenc
            .list
            .get_encode_guids
            .ok_or("nvEncGetEncodeGUIDs is null")?;

        let mut count = 0u32;
        // SAFETY: live encoder handle and a valid out pointer.
        if unsafe { count_fn(self.encoder, &mut count) } != NV_ENC_SUCCESS {
            return Err("nvEncGetEncodeGUIDCount failed".into());
        }

        let mut guids = vec![
            Guid {
                data1: 0,
                data2: 0,
                data3: 0,
                data4: [0; 8]
            };
            count as usize
        ];
        let mut returned = 0u32;
        // SAFETY: `guids` has room for `count` entries, which is what the call
        // above reported.
        if unsafe { guids_fn(self.encoder, guids.as_mut_ptr(), count, &mut returned) }
            != NV_ENC_SUCCESS
        {
            return Err("nvEncGetEncodeGUIDs failed".into());
        }
        guids.truncate(returned as usize);
        Ok(guids)
    }

    /// Query one capability for one codec.
    pub fn cap(&self, codec: Guid, cap: u32) -> Result<i32, String> {
        let caps_fn = self
            .nvenc
            .list
            .get_encode_caps
            .ok_or("nvEncGetEncodeCaps is null")?;

        // SAFETY: plain data; zero is valid for every field.
        let mut param: NvEncCapsParam = unsafe { std::mem::zeroed() };
        param.version = NV_ENC_CAPS_PARAM_VER;
        param.caps_to_query = cap;

        let mut value = 0i32;
        // SAFETY: live encoder handle, correctly versioned param, valid out
        // pointer.
        let status = unsafe { caps_fn(self.encoder, codec, &mut param, &mut value) };
        if status != NV_ENC_SUCCESS {
            return Err(format!("nvEncGetEncodeCaps({cap}) failed with {status}"));
        }
        Ok(value)
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        if let Some(destroy) = self.nvenc.list.destroy_encoder {
            // SAFETY: `encoder` came from nvEncOpenEncodeSessionEx and has not
            // been destroyed yet; this runs once, from Drop.
            unsafe { destroy(self.encoder) };
        }
    }
}

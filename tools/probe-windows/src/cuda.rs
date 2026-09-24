// SPDX-License-Identifier: GPL-2.0-or-later

//! The CUDA driver API, loaded at runtime.
//!
//! Only what `NvFBCToCuda` needs: adopt the context NvFBC made, allocate a
//! device buffer to grab into, and pull back a few kilobytes to tell one frame
//! from another.
//!
//! There is deliberately no `cuCtxDestroy` here. NvFBC creates the context when
//! it is not given one, so destroying it is not ours to do — and doing it anyway
//! is what killed the first run of this probe.
//!
//! # Why this costs nothing
//!
//! `nvcuda.dll` is the *driver* API and ships with every NVIDIA driver, so this
//! is a `LoadLibrary` at startup and no build dependency at all — the same shape
//! [`crate::nvenc`] already uses. There is no CUDA toolkit involved and
//! `cargo xwin` is unaffected.
//!
//! # The `_v2` trap
//!
//! `cuda.h` `#define`s most of these names to a `_v2` variant — `cuMemAlloc`
//! really resolves to `cuMemAlloc_v2` — so asking `GetProcAddress` for the bare
//! name returns null and CUDA looks absent rather than misnamed. That is the
//! same class of mistake as looking for `NvFBCCreateInstance` and concluding
//! NvFBC was unavailable, which cost this investigation several rounds. So each
//! symbol is resolved `_v2`-first with a bare-name fallback, and the probe
//! **prints which spelling answered** rather than leaving it to be assumed.

use std::ffi::{CString, c_void};

use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
use windows::core::PCSTR;

/// `CUdeviceptr` is 64-bit on x64, and is an integer rather than a pointer.
pub type CuDevicePtr = u64;
/// `CUcontext`.
pub type CuContext = *mut c_void;

pub const CUDA_SUCCESS: i32 = 0;

type PfnInit = unsafe extern "system" fn(u32) -> i32;
type PfnCtxPopCurrent = unsafe extern "system" fn(*mut CuContext) -> i32;
type PfnCtxPushCurrent = unsafe extern "system" fn(CuContext) -> i32;
type PfnMemAlloc = unsafe extern "system" fn(*mut CuDevicePtr, usize) -> i32;
type PfnMemFree = unsafe extern "system" fn(CuDevicePtr) -> i32;
type PfnMemcpyDtoH = unsafe extern "system" fn(*mut c_void, CuDevicePtr, usize) -> i32;

pub struct Cuda {
    init: PfnInit,
    ctx_pop: PfnCtxPopCurrent,
    ctx_push: PfnCtxPushCurrent,
    mem_alloc: PfnMemAlloc,
    mem_free: PfnMemFree,
    memcpy_dtoh: PfnMemcpyDtoH,
    /// Which spelling each symbol answered to, for the probe to print.
    pub resolved: Vec<(&'static str, &'static str)>,
}

fn symbol(module: HMODULE, name: &str) -> Option<*const c_void> {
    let cname = CString::new(name).ok()?;
    // SAFETY: `module` is live and `cname` is NUL-terminated.
    unsafe { GetProcAddress(module, PCSTR(cname.as_ptr().cast())) }.map(|p| p as *const c_void)
}

/// Resolve `name`, preferring the `_v2` spelling the headers actually alias to.
fn resolve(
    module: HMODULE,
    name: &'static str,
    resolved: &mut Vec<(&'static str, &'static str)>,
) -> Option<*const c_void> {
    if let Some(p) = symbol(module, &format!("{name}_v2")) {
        resolved.push((name, "_v2"));
        return Some(p);
    }
    if let Some(p) = symbol(module, name) {
        resolved.push((name, "bare"));
        return Some(p);
    }
    resolved.push((name, "MISSING"));
    None
}

impl Cuda {
    pub fn load() -> Result<Cuda, String> {
        let cname = CString::new("nvcuda.dll").map_err(|e| e.to_string())?;
        // SAFETY: `cname` is NUL-terminated and outlives the call.
        let module = unsafe { LoadLibraryA(PCSTR(cname.as_ptr().cast())) }
            .map_err(|e| format!("nvcuda.dll did not load: {e}"))?;

        let mut resolved = Vec::new();
        let mut need = |name: &'static str| resolve(module, name, &mut resolved);

        // cuInit has no _v2 form, but going through the same path keeps it in
        // the printed table.
        let init = need("cuInit");
        let ctx_pop = need("cuCtxPopCurrent");
        let ctx_push = need("cuCtxPushCurrent");
        let mem_alloc = need("cuMemAlloc");
        let mem_free = need("cuMemFree");
        let memcpy_dtoh = need("cuMemcpyDtoH");

        let missing: Vec<_> = resolved
            .iter()
            .filter(|(_, how)| *how == "MISSING")
            .map(|(name, _)| *name)
            .collect();
        if !missing.is_empty() {
            return Err(format!("nvcuda.dll is missing {}", missing.join(", ")));
        }

        // SAFETY: every signature is transcribed from the CUDA driver API, and
        // each pointer is non-null or the check above would have returned.
        unsafe {
            Ok(Cuda {
                init: crate::nvfbc::cast_fn(init.unwrap()),
                ctx_pop: crate::nvfbc::cast_fn(ctx_pop.unwrap()),
                ctx_push: crate::nvfbc::cast_fn(ctx_push.unwrap()),
                mem_alloc: crate::nvfbc::cast_fn(mem_alloc.unwrap()),
                mem_free: crate::nvfbc::cast_fn(mem_free.unwrap()),
                memcpy_dtoh: crate::nvfbc::cast_fn(memcpy_dtoh.unwrap()),
                resolved,
            })
        }
    }

    pub fn init(&self) -> i32 {
        // SAFETY: takes a flags word by value; 0 is the only defined value.
        unsafe { (self.init)(0) }
    }

    /// Take the current context off the stack, which is how NvFBC's own context
    /// is obtained when it was left to create one.
    pub fn ctx_pop(&self) -> Result<CuContext, i32> {
        let mut ctx: CuContext = std::ptr::null_mut();
        // SAFETY: `ctx` is a valid out pointer.
        let status = unsafe { (self.ctx_pop)(&mut ctx) };
        if status == CUDA_SUCCESS {
            Ok(ctx)
        } else {
            Err(status)
        }
    }

    pub fn ctx_push(&self, ctx: CuContext) -> i32 {
        // SAFETY: `ctx` came from `ctx_pop` and is still live.
        unsafe { (self.ctx_push)(ctx) }
    }

    pub fn mem_alloc(&self, bytes: usize) -> Result<CuDevicePtr, i32> {
        let mut ptr: CuDevicePtr = 0;
        // SAFETY: `ptr` is a valid out pointer and `bytes` is a real size.
        let status = unsafe { (self.mem_alloc)(&mut ptr, bytes) };
        if status == CUDA_SUCCESS {
            Ok(ptr)
        } else {
            Err(status)
        }
    }

    pub fn mem_free(&self, ptr: CuDevicePtr) -> i32 {
        // SAFETY: `ptr` came from `mem_alloc` and is freed exactly once.
        unsafe { (self.mem_free)(ptr) }
    }

    /// Copy a small slice back to host memory.
    ///
    /// Only ever used on a few kilobytes, to tell one captured frame from
    /// another. Pulling the whole frame back would reintroduce exactly the
    /// sysmem copy this interface exists to avoid.
    pub fn memcpy_dtoh(&self, dst: &mut [u8], src: CuDevicePtr) -> i32 {
        // SAFETY: `dst` is a valid writable slice of the declared length, and
        // `src` is a live device allocation of at least that many bytes.
        unsafe { (self.memcpy_dtoh)(dst.as_mut_ptr().cast(), src, dst.len()) }
    }
}

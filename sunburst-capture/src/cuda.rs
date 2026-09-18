// SPDX-License-Identifier: GPL-2.0-or-later

//! The CUDA driver API, loaded at runtime.
//!
//! Only what `NvFBCToCuda` needs: adopt the context NvFBC made and allocate the
//! device buffer to grab into.
//!
//! There is deliberately no `cuCtxDestroy` here. NvFBC creates the context when
//! it is not given one, so destroying it is not ours to do — and doing it anyway
//! is what killed the first run of the Phase-0 probe this was promoted from.
//!
//! # Why this costs nothing
//!
//! `nvcuda.dll` is the *driver* API and ships with every NVIDIA driver, so this
//! is a `LoadLibrary` at startup and no build dependency at all. There is no CUDA
//! toolkit involved and `cargo xwin` is unaffected.
//!
//! # The `_v2` trap
//!
//! `cuda.h` `#define`s most of these names to a `_v2` variant — `cuMemAlloc`
//! really resolves to `cuMemAlloc_v2` — so asking `GetProcAddress` for the bare
//! name returns null and CUDA looks absent rather than misnamed. That is the
//! same class of mistake as looking for `NvFBCCreateInstance` and concluding
//! NvFBC was unavailable. So each symbol is resolved `_v2`-first with a bare-name
//! fallback, and a symbol that answers to neither spelling fails the load.

// This module is a thin wrapper over the CUDA driver API: the CUcontext /
// CUmodule / CUfunction pointers it takes are opaque handles whose validity is
// the caller's contract (documented per method), not something it dereferences.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{CString, c_char, c_void};

use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
use windows::core::PCSTR;

/// `CUdeviceptr` is 64-bit on x64, and is an integer rather than a pointer.
pub type CuDevicePtr = u64;
/// `CUcontext`.
pub type CuContext = *mut c_void;
/// `CUmodule`.
pub type CuModule = *mut c_void;
/// `CUfunction`.
pub type CuFunction = *mut c_void;

pub const CUDA_SUCCESS: i32 = 0;

type PfnInit = unsafe extern "system" fn(u32) -> i32;
type PfnCtxPopCurrent = unsafe extern "system" fn(*mut CuContext) -> i32;
type PfnCtxPushCurrent = unsafe extern "system" fn(CuContext) -> i32;
type PfnMemAlloc = unsafe extern "system" fn(*mut CuDevicePtr, usize) -> i32;
type PfnMemFree = unsafe extern "system" fn(CuDevicePtr) -> i32;
type PfnModuleLoadData = unsafe extern "system" fn(*mut CuModule, *const c_void) -> i32;
type PfnModuleGetFunction =
    unsafe extern "system" fn(*mut CuFunction, CuModule, *const c_char) -> i32;
type PfnLaunchKernel = unsafe extern "system" fn(
    CuFunction,
    u32,
    u32,
    u32, // grid x/y/z
    u32,
    u32,
    u32,              // block x/y/z
    u32,              // shared mem bytes
    *mut c_void,      // stream
    *mut *mut c_void, // kernel params
    *mut *mut c_void, // extra
) -> i32;
type PfnCtxSynchronize = unsafe extern "system" fn() -> i32;

pub struct Cuda {
    init: PfnInit,
    ctx_pop: PfnCtxPopCurrent,
    ctx_push: PfnCtxPushCurrent,
    mem_alloc: PfnMemAlloc,
    mem_free: PfnMemFree,
    module_load_data: PfnModuleLoadData,
    module_get_function: PfnModuleGetFunction,
    launch_kernel: PfnLaunchKernel,
    ctx_synchronize: PfnCtxSynchronize,
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

        // cuInit has no _v2 form, but going through the same path keeps the
        // `_v2`-first resolution uniform.
        let init = need("cuInit");
        let ctx_pop = need("cuCtxPopCurrent");
        let ctx_push = need("cuCtxPushCurrent");
        let mem_alloc = need("cuMemAlloc");
        let mem_free = need("cuMemFree");
        let module_load_data = need("cuModuleLoadData");
        let module_get_function = need("cuModuleGetFunction");
        let launch_kernel = need("cuLaunchKernel");
        let ctx_synchronize = need("cuCtxSynchronize");

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
                module_load_data: crate::nvfbc::cast_fn(module_load_data.unwrap()),
                module_get_function: crate::nvfbc::cast_fn(module_get_function.unwrap()),
                launch_kernel: crate::nvfbc::cast_fn(launch_kernel.unwrap()),
                ctx_synchronize: crate::nvfbc::cast_fn(ctx_synchronize.unwrap()),
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

    /// Load a PTX (NUL-terminated text) or cubin image into a module — the driver
    /// JITs PTX, so no toolkit is needed at runtime.
    pub fn module_load_data(&self, image: &[u8]) -> Result<CuModule, i32> {
        let mut module: CuModule = std::ptr::null_mut();
        // SAFETY: `image` outlives the call; `module` is a valid out pointer.
        let status = unsafe { (self.module_load_data)(&mut module, image.as_ptr().cast()) };
        if status == CUDA_SUCCESS {
            Ok(module)
        } else {
            Err(status)
        }
    }

    /// Get a kernel entry point by name from a loaded module.
    pub fn module_get_function(&self, module: CuModule, name: &str) -> Result<CuFunction, i32> {
        let cname = CString::new(name).map_err(|_| -1)?;
        let mut f: CuFunction = std::ptr::null_mut();
        // SAFETY: `module` is live; `cname` is NUL-terminated; `f` is a valid out.
        let status = unsafe { (self.module_get_function)(&mut f, module, cname.as_ptr()) };
        if status == CUDA_SUCCESS {
            Ok(f)
        } else {
            Err(status)
        }
    }

    /// Launch `f` over a `grid` of `block` threads. `params` holds one pointer per
    /// kernel argument, in order.
    pub fn launch_kernel(
        &self,
        f: CuFunction,
        grid: (u32, u32, u32),
        block: (u32, u32, u32),
        params: &mut [*mut c_void],
    ) -> i32 {
        // SAFETY: `f` is a live function and `params` points at each of its
        // arguments, matching the kernel's signature (the caller's contract).
        unsafe {
            (self.launch_kernel)(
                f,
                grid.0,
                grid.1,
                grid.2,
                block.0,
                block.1,
                block.2,
                0,
                std::ptr::null_mut(),
                params.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        }
    }

    /// Block until all work on the current context completes.
    pub fn ctx_synchronize(&self) -> i32 {
        // SAFETY: no arguments; synchronises the current context.
        unsafe { (self.ctx_synchronize)() }
    }
}

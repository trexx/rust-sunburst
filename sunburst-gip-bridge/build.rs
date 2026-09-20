// SPDX-License-Identifier: GPL-2.0-or-later

//! Builds the vendored xow driver + libusb, but only when the `vendored` feature
//! is on and the target is Android — the one place the bridge runs and the one
//! build with the NDK toolchain. Every other build (the Linux dev host, CI, the
//! Windows cross-check) does nothing here and links the pure-Rust stub instead.

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=vendor");
    println!("cargo:rerun-if-changed=build.rs");

    let vendored = std::env::var_os("CARGO_FEATURE_VENDORED").is_some();
    let android = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android");
    if !(vendored && android) {
        return;
    }

    build_libusb();
    build_driver();
}

/// Compile libusb from the vendored source, the same set and flags the upstream
/// Android `libusb.mk` uses: only the linux + posix backends, the pre-generated
/// `android/config.h`, hidden visibility.
fn build_libusb() {
    let root = Path::new("vendor/libusb");
    let src = root.join("libusb");

    let files = [
        "core.c",
        "descriptor.c",
        "hotplug.c",
        "io.c",
        "sync.c",
        "strerror.c",
        "os/linux_usbfs.c",
        "os/events_posix.c",
        "os/threads_posix.c",
        "os/linux_netlink.c",
    ];

    let mut build = cc::Build::new();
    build
        .include(&src) // libusbi.h and the public header
        .include(src.join("os")) // the backend headers
        .include(root.join("android")) // config.h
        .flag("-fvisibility=hidden")
        .flag("-pthread")
        // libusb's own warnings are not ours to fix; keep them quiet without
        // hiding warnings in code we do own.
        .warnings(false);
    for f in files {
        build.file(src.join(f));
    }
    build.compile("usb1.0");

    // The netlink hotplug backend and the fd path both need -llog on Android.
    println!("cargo:rustc-link-lib=log");
}

/// Compile the de-JNI'd xow driver (`vendor/xow`) plus the Sunburst shim
/// (`vendor/shim`) as C++17, over libusb. The shim's `crypto.cpp` calls the
/// `sb_crypto_*` symbols from `src/crypto.rs`, resolved at the final cdylib link.
fn build_driver() {
    let xow = Path::new("vendor/xow");
    let shim = Path::new("vendor/shim");

    let driver = [
        xow.join("dongle/usb.cpp"),
        xow.join("dongle/mt76.cpp"),
        xow.join("dongle/dongle.cpp"),
        xow.join("wired/usb_wired.cpp"),
        xow.join("wired/wired.cpp"),
        xow.join("utils/log.cpp"),
        xow.join("controller/controller.cpp"),
        xow.join("controller/gip.cpp"),
        shim.join("crypto.cpp"),
        shim.join("sb_gip_shim.cpp"),
    ];

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .include(xow) // the driver's own `../utils/...` etc.
        .include(shim) // sb_gip_shim.h
        .include("vendor/libusb") // <libusb/libusb.h>
        .cpp_link_stdlib("c++_static")
        // Vendored code; its warnings are not ours to fix, and it is not linted
        // here (that is the Rust side's job).
        .warnings(false);
    for f in driver {
        build.file(f);
    }
    build.compile("xowdriver");

    // `cpp_link_stdlib("c++_static")` links `libc++_static.a` (the STL) but not
    // the C++ ABI runtime (`__cxa_*`) or the unwinder (`_Unwind_*`), which on the
    // NDK are separate libs. Emit them with the default kind (like `-llog`) so the
    // NDK linker resolves them from its own sysroot rather than rustc's `-L` paths,
    // and after the STL so the archive referencing those symbols precedes them.
    println!("cargo:rustc-link-lib=c++abi");
    println!("cargo:rustc-link-lib=unwind");
}

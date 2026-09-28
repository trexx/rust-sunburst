// SPDX-License-Identifier: GPL-2.0-or-later
#![cfg(target_os = "android")]

//! The Xbox pad bridge's JNI seam and its process-global handle.
//!
//! Kotlin owns the USB permission dance and hands down a claimed device's file
//! descriptor; `nativeUsbAttach` opens the vendored driver on it
//! ([`sunburst_gip_bridge::Bridge`]) and parks the handle in a process global so
//! the pieces that need it can reach it independently of each other's lifetime:
//! the UI thread (pairing), the client loop (poll, B3) and the pad sink (rumble,
//! B3). The handle is an `Arc<Bridge>` because the bridge is `Sync` — the C++
//! shim serialises access — so those callers share one without a Rust-side lock
//! around the driver itself.

use std::sync::{Arc, Mutex, OnceLock};

use jni::EnvUnowned;
use jni::errors::LogErrorAndDefault;
use jni::objects::{JObject, JString};
use jni::sys::{jboolean, jint};

use sunburst_gip_bridge::Bridge;

/// The currently-attached device's bridge, or `None`. Written by attach/detach on
/// the UI thread; read by the client loop and the pad sink.
static BRIDGE: OnceLock<Mutex<Option<Arc<Bridge>>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<Arc<Bridge>>> {
    BRIDGE.get_or_init(|| Mutex::new(None))
}

/// The current bridge, if a pad/adapter is attached. Cloned out so the caller
/// (the client loop's poll, the pad sink's rumble — B3) holds it without keeping
/// the global locked.
pub fn current_bridge() -> Option<Arc<Bridge>> {
    slot().lock().expect("not poisoned").clone()
}

/// The Xbox Wireless Adapter product ids (Microsoft VID `0x045e`): old, new, and
/// the Surface built-in. Anything else that matched the USB filter is a wired pad.
fn is_adapter(pid: i32) -> bool {
    matches!(pid as u16, 0x02e6 | 0x02fe | 0x091e)
}

/// Open the driver on a claimed device fd. `pid` selects adapter vs wired;
/// `firmware_path` is only used for the adapter. Returns true on success.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_trexx_sunburst_UsbBridge_nativeUsbAttach<'local>(
    mut env: EnvUnowned<'local>,
    _obj: JObject<'local>,
    fd: jint,
    _vid: jint,
    pid: jint,
    firmware_path: JString<'local>,
) -> jboolean {
    let firmware: String = env
        .with_env(|env| firmware_path.try_to_string(env))
        .resolve::<LogErrorAndDefault>();

    let adapter = is_adapter(pid);
    let opened = if adapter {
        Bridge::open_dongle(fd, &firmware)
    } else {
        Bridge::open_wired(fd)
    };

    match opened {
        Ok(bridge) => {
            *slot().lock().expect("not poisoned") = Some(Arc::new(bridge));
            log::info!(
                "gip: opened {} (pid {:04x})",
                if adapter { "adapter" } else { "wired pad" },
                pid as u16
            );
            true
        }
        Err(e) => {
            log::error!("gip: open failed: {e}");
            false
        }
    }
}

/// Drop the current bridge, closing the device.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_trexx_sunburst_UsbBridge_nativeUsbDetach(
    _env: EnvUnowned,
    _obj: JObject,
) {
    *slot().lock().expect("not poisoned") = None;
    log::info!("gip: detached");
}

/// Put the adapter in (or out of) pairing mode. No-op without an adapter.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_trexx_sunburst_UsbBridge_nativeSetPairing(
    _env: EnvUnowned,
    _obj: JObject,
    on: jboolean,
) -> jboolean {
    match current_bridge() {
        Some(bridge) => bridge.set_pairing(on),
        None => false,
    }
}

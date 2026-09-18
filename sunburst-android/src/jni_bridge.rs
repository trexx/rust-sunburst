// SPDX-License-Identifier: GPL-2.0-or-later
#![cfg(target_os = "android")]

//! The JNI boundary Kotlin calls across.
//!
//! `StreamActivity` starts the client when its `SurfaceView` is ready and stops
//! it when the surface goes away. Each `nativeStart` returns an opaque handle
//! (a boxed [`Client`]); `nativeStop` consumes it. The frame-path threads the
//! client owns are built in the session commit — this establishes the boundary,
//! the logging, and the handle lifecycle.

use std::sync::Once;

use jni::JNIEnv;
use jni::objects::{JClass, JObject, JString};
use jni::sys::{jint, jlong};

/// One running (or being-built) client. Fields land with the session commit;
/// today it exists so the handle lifecycle is real from the start.
pub struct Client {
    server: String,
}

static LOG_INIT: Once = Once::new();

fn init_logging() {
    LOG_INIT.call_once(|| {
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Info)
                .with_tag("sunburst"),
        );
    });
}

/// Start the client against `host:port`, rendering into `surface`. Returns an
/// opaque handle, or 0 on failure. The handle is owned by the caller until
/// [`nativeStop`](Java_com_trexx_sunburst_StreamActivity_nativeStop).
///
/// # Safety
/// Called only by the JVM with valid arguments; the returned pointer must be
/// passed back exactly once to `nativeStop`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_trexx_sunburst_StreamActivity_nativeStart(
    mut env: JNIEnv,
    _class: JClass,
    _surface: JObject,
    host: JString,
    port: jint,
) -> jlong {
    init_logging();
    let host: String = match env.get_string(&host) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };
    let server = format!("{host}:{port}");
    log::info!("nativeStart: client for {server}");
    // The frame-path threads are spawned in the session commit; for now the
    // handle is real so start/stop is exercised end to end on-device.
    let client = Box::new(Client { server });
    Box::into_raw(client) as jlong
}

/// Stop and free a client started by `nativeStart`.
///
/// # Safety
/// `handle` must be a value returned by `nativeStart` and not yet stopped.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_trexx_sunburst_StreamActivity_nativeStop(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    if handle == 0 {
        return;
    }
    // SAFETY: `handle` came from `Box::into_raw` in `nativeStart` and is consumed
    // exactly once (the Kotlin side zeroes its copy after calling stop).
    let client = unsafe { Box::from_raw(handle as *mut Client) };
    log::info!("nativeStop: client for {}", client.server);
}

/// The `SurfaceView` surface changed (size or identity); rebind the decoder
/// output. A no-op until the decoder exists (session commit).
///
/// # Safety
/// `handle` must be a live handle from `nativeStart`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_trexx_sunburst_StreamActivity_nativeSurfaceChanged(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    _surface: JObject,
) {
    if handle == 0 {
        return;
    }
    log::info!("nativeSurfaceChanged");
}

// SPDX-License-Identifier: GPL-2.0-or-later
#![cfg(target_os = "android")]

//! The JNI boundary Kotlin calls across.
//!
//! `StreamActivity` starts the client when its `SurfaceView` is ready and stops
//! it when the surface goes away. `nativeStart` acquires the `ANativeWindow` from
//! the Java `Surface`, spawns the client thread, and returns an opaque handle (a
//! boxed [`Client`]); `nativeStop` signals the thread and joins it.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;

use jni::JNIEnv;
use jni::objects::{JClass, JObject, JString};
use jni::sys::{jint, jlong};
use ndk::native_window::NativeWindow;

use crate::client;
use crate::input_map::ClientInput;

/// A running client: the stop flag its thread polls, the join handle, and the
/// channel input callbacks push onto (drained and sent by the client thread, so
/// the socket is touched from one place).
pub struct Client {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    input_tx: Sender<ClientInput>,
}

impl Client {
    /// Enqueue a raw input event for the client thread to map and send.
    pub fn push_input(&self, input: ClientInput) {
        let _ = self.input_tx.send(input);
    }
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

/// Parse a 64-character hex pairing secret into 32 bytes.
fn parse_secret(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Start the client against `host:port` with `secretHex` (the paired secret) and
/// the codecs the device can decode, rendering into `surface`. Returns an opaque
/// handle, or 0 on failure — passed back exactly once to `nativeStop`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_trexx_sunburst_StreamActivity_nativeStart(
    mut env: JNIEnv,
    _class: JClass,
    surface: JObject,
    host: JString,
    port: jint,
    secret_hex: JString,
    codecs: jint,
) -> jlong {
    init_logging();

    let host: String = match env.get_string(&host) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };
    let secret_hex: String = match env.get_string(&secret_hex) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };
    let Some(secret) = parse_secret(&secret_hex) else {
        log::error!("no valid pairing secret; pair the device first");
        return 0;
    };
    let server: SocketAddr = match format!("{host}:{port}").parse() {
        Ok(a) => a,
        Err(e) => {
            log::error!("bad server address {host}:{port}: {e}");
            return 0;
        }
    };

    // Acquire the native window from the Java Surface. The `jni` crate and `ndk`
    // pull different `jni-sys` versions whose types are the stable JNI ABI, so
    // the raw pointers are reinterpreted with `cast`.
    // SAFETY: `env` and `surface` are the live JVM env and Surface for this call.
    let window =
        unsafe { NativeWindow::from_surface(env.get_raw().cast(), surface.as_raw().cast()) };
    let Some(window) = window else {
        log::error!("ANativeWindow_fromSurface returned null");
        return 0;
    };

    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let codecs = codecs as u8;
    let (input_tx, input_rx) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("sunburst-client".into())
        .spawn(move || client::run(server, secret, codecs, window, thread_stop, input_rx))
        .ok();

    log::info!("client started for {server}");
    Box::into_raw(Box::new(Client {
        stop,
        thread,
        input_tx,
    })) as jlong
}

/// Stop and free a client started by `nativeStart`.
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
    let mut client = unsafe { Box::from_raw(handle as *mut Client) };
    client.stop.store(true, Ordering::Relaxed);
    if let Some(t) = client.thread.take() {
        let _ = t.join();
    }
    log::info!("client stopped");
}

/// The surface changed. The client is torn down and rebuilt by the Activity's
/// destroy/create around a real surface change, so this only logs.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_trexx_sunburst_StreamActivity_nativeSurfaceChanged(
    _env: JNIEnv,
    _class: JClass,
    _handle: jlong,
    _surface: JObject,
) {
    log::info!("surface changed");
}

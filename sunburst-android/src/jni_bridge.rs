// SPDX-License-Identifier: GPL-2.0-or-later
#![cfg(target_os = "android")]

//! The JNI boundary Kotlin calls across.
//!
//! `StreamActivity` starts the client when its `SurfaceView` is ready and stops
//! it when the surface goes away. `nativeStart` acquires the `ANativeWindow` from
//! the Java `Surface`, spawns the client thread, and returns an opaque handle (a
//! boxed [`Client`]); `nativeStop` signals the thread and joins it.

use std::net::SocketAddr;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::client::{Callbacks, StreamPrefs};
use jni::JNIEnv;
use jni::objects::{JClass, JObject, JString};
use jni::sys::{jint, jlong};
use ndk::native_window::NativeWindow;
use sunburst_core::proto::StreamCodec;

use crate::client;
use crate::cursor_predict::{CursorPredictor, CursorShared, SubPixel};
use crate::input_map::ClientInput;

/// A running client: the stop flag its thread polls, the join handle, and the
/// channel input callbacks push onto (drained and sent by the client thread, so
/// the socket is touched from one place).
pub struct Client {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    input_tx: Sender<ClientInput>,
    /// The client thread's OS tid, set once it starts, so the Java
    /// PerformanceHintManager can target that thread. 0 until set.
    client_tid: Arc<AtomicI32>,
    /// Shared with the client thread: input numbering, geometry, and the
    /// server's latest cursor report.
    pub cursor: Arc<CursorShared>,
    /// The cursor prediction. Locked only from the UI thread (the JNI input and
    /// cursor-sync calls), so never contended, and never by the client thread.
    pub ui_cursor: Mutex<(SubPixel, CursorPredictor)>,
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
///
/// `prefer_codec` is a `StreamCodec` discriminant (0 HEVC, 1 AV1, 2 H.264) or
/// `-1` for "let the server choose"; `max_bitrate_kbps` is a client-side ceiling
/// (`0` = none); `jitter_min_ms` is the jitter-buffer floor. All three come from
/// the TV settings screen.
#[allow(clippy::too_many_arguments)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_trexx_sunburst_StreamActivity_nativeStart(
    mut env: JNIEnv,
    activity: JObject,
    surface: JObject,
    host: JString,
    port: jint,
    secret_hex: JString,
    codecs: jint,
    prefer_codec: jint,
    max_bitrate_kbps: jint,
    jitter_min_ms: jint,
    audio_route: jint,
    pad_volume: jint,
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

    // Capture the JVM and a global ref to the activity for cursor upcalls.
    let Ok(vm) = env.get_java_vm() else {
        log::error!("no JavaVM");
        return 0;
    };
    let Ok(activity_ref) = env.new_global_ref(&activity) else {
        log::error!("global ref failed");
        return 0;
    };
    let callbacks = Callbacks {
        vm,
        activity: activity_ref,
    };

    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let codecs = codecs as u8;
    let prefs = StreamPrefs {
        prefer_codec: u8::try_from(prefer_codec)
            .ok()
            .and_then(StreamCodec::from_u8),
        max_bitrate_kbps: max_bitrate_kbps.max(0) as u32,
        jitter_min_ms: jitter_min_ms.max(0) as u32,
        audio_route: audio_route.clamp(0, 2) as u8,
        pad_volume: pad_volume.clamp(0, 100) as u8,
    };
    let (input_tx, input_rx) = mpsc::channel();
    let cursor = Arc::new(CursorShared::default());
    let thread_cursor = Arc::clone(&cursor);
    let client_tid = Arc::new(AtomicI32::new(0));
    let thread_tid = Arc::clone(&client_tid);
    let thread = std::thread::Builder::new()
        .name("sunburst-client".into())
        .spawn(move || {
            client::run(
                server,
                secret,
                codecs,
                prefs,
                window,
                thread_stop,
                input_rx,
                thread_tid,
                thread_cursor,
                callbacks,
            )
        })
        .ok();

    log::info!("client started for {server}");
    Box::into_raw(Box::new(Client {
        stop,
        thread,
        input_tx,
        client_tid,
        cursor,
        ui_cursor: Mutex::new((SubPixel::default(), CursorPredictor::new())),
    })) as jlong
}

/// The client thread's OS tid, for the Java PerformanceHintManager to target.
/// 0 until the thread has started.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_trexx_sunburst_StreamActivity_nativeClientTid(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jint {
    if handle == 0 {
        return 0;
    }
    // SAFETY: `handle` is a live `Client` from `nativeStart`.
    let client = unsafe { &*(handle as *const Client) };
    client.client_tid.load(Ordering::Relaxed)
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

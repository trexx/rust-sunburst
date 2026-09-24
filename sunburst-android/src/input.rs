// SPDX-License-Identifier: GPL-2.0-or-later
#![cfg(target_os = "android")]

//! JNI entry points for input. Kotlin's View callbacks forward raw Android
//! events here; each pushes a [`ClientInput`] onto the client's input channel,
//! which the client thread drains, maps, and sends as authenticated input
//! packets. Keeping the send on the client thread means the socket is touched
//! from one place.

use jni::JNIEnv;
use jni::objects::JClass;
use jni::sys::{jboolean, jfloat, jint, jlong};

use crate::cursor_predict::pack;
use crate::input_map::ClientInput;
use crate::jni_bridge::Client;

/// Borrow the client behind a handle and enqueue an input event. The handle is
/// live between `nativeStart` and `nativeStop`, and Kotlin serialises input and
/// teardown on the UI thread, so the borrow does not outlive the box.
fn enqueue(handle: jlong, input: ClientInput) {
    if handle == 0 {
        return;
    }
    // SAFETY: `handle` is a live `Client` from `nativeStart`; input callbacks and
    // `nativeStop` are both on the UI thread, so it is not freed concurrently.
    let client = unsafe { &*(handle as *const Client) };
    client.push_input(input);
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_trexx_sunburst_StreamActivity_nativeKey(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    code: jint,
    down: jboolean,
    meta: jint,
) {
    enqueue(
        handle,
        ClientInput::Key {
            code,
            down: down != 0,
            meta,
        },
    );
}

/// CLOCK_MONOTONIC milliseconds, for the prediction's "still moving" window.
fn now_ms() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable out-param for clock_gettime.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    #[allow(clippy::unnecessary_cast)] // narrower fields on armv7
    let ms = ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000;
    ms
}

/// A captured-pointer move (the sum of the event's batched history, in
/// fractional counts): send the whole counts, numbered here so the prediction
/// can match them to the server's reports, and return where the overlay should
/// be drawn now — or -1 while not predicting, when the overlay follows the
/// server's reports as before.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_trexx_sunburst_StreamActivity_nativeMouseMove(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    dx: jfloat,
    dy: jfloat,
) -> jlong {
    if handle == 0 {
        return -1;
    }
    // SAFETY: as `enqueue`: a live `Client`, freed only on the UI thread.
    let client = unsafe { &*(handle as *const Client) };
    let now = now_ms();
    let Ok(mut guard) = client.ui_cursor.lock() else {
        return -1;
    };
    let (sub, predictor) = &mut *guard;
    client.cursor.sync(predictor, now);
    let (ix, iy) = sub.take(dx, dy);
    if ix != 0 || iy != 0 {
        let seq = client.cursor.next_seq();
        client.push_input(ClientInput::MouseRel {
            seq,
            dx: ix,
            dy: iy,
        });
        predictor.on_local_move(seq, ix, iy, now);
    }
    pack(predictor.position())
}

/// A server cursor report arrived (the client thread's upcall, marshalled to
/// the UI thread): fold it into the prediction and return the overlay position,
/// or -1 while not predicting.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_trexx_sunburst_StreamActivity_nativeCursorSync(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jlong {
    if handle == 0 {
        return -1;
    }
    // SAFETY: as `enqueue`.
    let client = unsafe { &*(handle as *const Client) };
    let Ok(mut guard) = client.ui_cursor.lock() else {
        return -1;
    };
    let (_, predictor) = &mut *guard;
    client.cursor.sync(predictor, now_ms());
    pack(predictor.position())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_trexx_sunburst_StreamActivity_nativeMouseButton(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    code: jint,
    down: jboolean,
) {
    enqueue(
        handle,
        ClientInput::MouseButton {
            code,
            down: down != 0,
        },
    );
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_trexx_sunburst_StreamActivity_nativeWheel(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    delta: jfloat,
    horizontal: jboolean,
) {
    enqueue(
        handle,
        ClientInput::Wheel {
            delta,
            horizontal: horizontal != 0,
        },
    );
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_trexx_sunburst_StreamActivity_nativePadButton(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    code: jint,
    down: jboolean,
) {
    enqueue(
        handle,
        ClientInput::PadButton {
            code,
            down: down != 0,
        },
    );
}

#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_com_trexx_sunburst_StreamActivity_nativePadAxis(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    lx: jfloat,
    ly: jfloat,
    rx: jfloat,
    ry: jfloat,
    lt: jfloat,
    rt: jfloat,
    hat_x: jfloat,
    hat_y: jfloat,
) {
    enqueue(
        handle,
        ClientInput::PadAxis {
            lx,
            ly,
            rx,
            ry,
            lt,
            rt,
            hat_x,
            hat_y,
        },
    );
}

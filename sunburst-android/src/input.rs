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

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_trexx_sunburst_StreamActivity_nativeMouseMove(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    dx: jfloat,
    dy: jfloat,
) {
    enqueue(handle, ClientInput::MouseMove { dx, dy });
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

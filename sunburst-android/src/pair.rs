// SPDX-License-Identifier: GPL-2.0-or-later
#![cfg(target_os = "android")]

//! Pairing from the TV, the same handshake as `fakeclient pair`.
//!
//! The client generates the PIN, shows it on the TV, and derives the shared
//! secret from it and both nonces; the user arms pairing in the web UI and types
//! that PIN there. The PIN never crosses the wire.
//!
//! Nothing is paired until the PIN is typed, so after confirming, this waits for
//! the server's `PairResult` — with the PIN still on screen — and hands the
//! secret back to Kotlin only on an acceptance whose tag proves the server
//! derived the same secret. Returning early is what left the TV holding a
//! secret the server never stored.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use jni::errors::LogErrorAndDefault;
use jni::objects::{JClass, JObject, JString, JValue};
use jni::sys::jint;
use jni::{Env, EnvUnowned, jni_sig, jni_str};
use sunburst_core::proto::pairing::{
    NONCE_LEN, PAIRING_WINDOW_SECS, accepted_tag, confirm_tag, derive_secret, tags_match,
};
use sunburst_core::proto::{ClientControl, DecoderQuirks, PairOutcome, PairRequest, ServerControl};
use sunburst_net::ClientEndpoint;

use crate::pin::generate_pin;

/// Set by `nativeCancelPair` (Back on the pair screen) to end the wait early.
/// One pairing runs at a time, so one flag serves.
static CANCEL: AtomicBool = AtomicBool::new(false);

/// How long to wait for the challenge. The server answers at once when armed.
const CHALLENGE_TIMEOUT: Duration = Duration::from_secs(5);

/// Generate a PIN to display. The client shows it; the user types it into the
/// web UI. Returns an empty string only if the OS RNG fails.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_trexx_sunburst_PairActivity_nativeGenPin<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> JString<'local> {
    let pin = generate_pin().unwrap_or_default();
    env.with_env(|env| env.new_string(pin))
        .resolve::<LogErrorAndDefault>()
}

/// Run the pairing handshake against `host:port` with the shown `pin`, and wait
/// for the server to decide. Returns the 64-char hex secret on success, or a
/// message saying why not — never 64 hex characters, so Kotlin tells them apart
/// by shape. Each wrong PIN typed in the web UI calls `onWrongPin(remaining)`
/// on the activity, from this (the calling) thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_trexx_sunburst_PairActivity_nativePair<'local>(
    mut env: EnvUnowned<'local>,
    activity: JObject<'local>,
    host: JString<'local>,
    port: jint,
    pin: JString<'local>,
) -> JString<'local> {
    env.with_env(|env| {
        let host = match host.try_to_string(env) {
            Ok(s) => s,
            Err(_) => return env.new_string("could not read the host"),
        };
        let pin = match pin.try_to_string(env) {
            Ok(s) => s,
            Err(_) => return env.new_string("could not read the PIN"),
        };
        CANCEL.store(false, Ordering::Relaxed);
        let result = pair(&host, port as u16, &pin, |remaining| {
            on_wrong_pin(env, &activity, remaining)
        });
        match result {
            Ok(secret) => {
                let hex: String = secret.iter().map(|b| format!("{b:02x}")).collect();
                env.new_string(hex)
            }
            Err(e) => {
                log::error!("pairing failed: {e}");
                env.new_string(e)
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

/// Tell the activity a wrong PIN was typed, with the tries left.
fn on_wrong_pin(env: &mut Env, activity: &JObject, remaining: u8) {
    let _ = env.call_method(
        activity,
        jni_str!("onWrongPin"),
        jni_sig!("(I)V"),
        &[JValue::Int(remaining.into())],
    );
    // A throwing callback must not poison the JNI calls that follow.
    if env.exception_check() {
        env.exception_clear();
    }
}

/// End a `nativePair` wait early. Called from the UI thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_trexx_sunburst_PairActivity_nativeCancelPair(
    _env: EnvUnowned,
    _class: JClass,
) {
    CANCEL.store(true, Ordering::Relaxed);
}

fn cancelled() -> Result<(), String> {
    if CANCEL.load(Ordering::Relaxed) {
        Err("pairing cancelled".into())
    } else {
        Ok(())
    }
}

/// The handshake: PairRequest -> PairChallenge -> PairConfirm, deriving the
/// secret from the PIN and both nonces, then the wait for `PairResult`. Quirks
/// are conservative here; the client refreshes them from the decoder probe once
/// streaming.
fn pair(
    host: &str,
    port: u16,
    pin: &str,
    mut on_wrong_pin: impl FnMut(u8),
) -> Result<[u8; 32], String> {
    let server: SocketAddr = format!("{host}:{port}")
        .parse()
        .map_err(|e| format!("{e}"))?;
    let mut client = ClientEndpoint::connect(server, None).map_err(|e| e.to_string())?;

    let mut client_nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut client_nonce).map_err(|e| e.to_string())?;

    client
        .send_control(&ClientControl::PairRequest(PairRequest {
            name: "sunburst-android".into(),
            model: android_model(),
            abi: std::env::consts::ARCH.into(),
            quirks: DecoderQuirks::default(),
            client_nonce,
        }))
        .map_err(|e| e.to_string())?;

    // Await the challenge, ticking so the reliable PairRequest is retransmitted.
    let deadline = Instant::now() + CHALLENGE_TIMEOUT;
    let (request_id, server_nonce) = loop {
        cancelled()?;
        if let Some(ServerControl::PairChallenge {
            request_id,
            server_nonce,
        }) = client.recv_control().map_err(|e| e.to_string())?
        {
            break (request_id, server_nonce);
        }
        client.tick().map_err(|e| e.to_string())?;
        if Instant::now() >= deadline {
            return Err("no answer from the server; arm pairing in the web UI first".into());
        }
    };

    // The server matches the confirm to its request by id, and ids count up
    // for the life of the server — so it is the challenge's, never a guess.
    let secret = derive_secret(pin, &client_nonce, &server_nonce);
    client
        .send_control(&ClientControl::PairConfirm {
            request_id,
            tag: confirm_tag(&secret),
        })
        .map_err(|e| e.to_string())?;

    // Now the person types the PIN. The arming closes within the window, so
    // the server has decided by then; a result arrives before that.
    let deadline = Instant::now() + Duration::from_secs(PAIRING_WINDOW_SECS);
    let expected = accepted_tag(&secret);
    loop {
        cancelled()?;
        if Instant::now() >= deadline {
            return Err("the PIN was not entered in time".into());
        }
        let message = client.recv_control().map_err(|e| e.to_string())?;
        client
            .tick()
            .map_err(|_| "lost contact with the server".to_string())?;
        let Some(ServerControl::PairResult {
            request_id: id,
            outcome,
        }) = message
        else {
            continue;
        };
        if id != request_id {
            continue;
        }
        match outcome {
            PairOutcome::Accepted { tag } if tags_match(&tag, &expected) => break,
            // The result carries no MAC. An acceptance without the tag did not
            // come from a server holding this secret; wait for the real one.
            PairOutcome::Accepted { .. } => {}
            PairOutcome::WrongPin { remaining } => on_wrong_pin(remaining),
            PairOutcome::Rejected => {
                return Err("pairing was cancelled or closed in the web UI".into());
            }
        }
    }

    // Keep answering briefly, so a resent result is acknowledged again rather
    // than retransmitted at an endpoint that has moved on.
    let until = Instant::now() + Duration::from_secs(1);
    while Instant::now() < until {
        let _ = client.recv_control();
        let _ = client.tick();
    }
    Ok(secret)
}

fn android_model() -> String {
    // Best-effort; the model also travels the JVM but this avoids a JNI call.
    "Android TV".into()
}

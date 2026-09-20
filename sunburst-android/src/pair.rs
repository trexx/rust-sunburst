// SPDX-License-Identifier: GPL-2.0-or-later
#![cfg(target_os = "android")]

//! Pairing from the TV, the same handshake as `fakeclient pair`.
//!
//! The client generates the PIN, shows it on the TV, and derives the shared
//! secret from it and both nonces; the user arms pairing in the web UI and types
//! that PIN there. The PIN never crosses the wire. The derived secret is handed
//! back to Kotlin to store in app-private prefs.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use jni::JNIEnv;
use jni::objects::{JClass, JString};
use jni::sys::{jint, jstring};
use sunburst_core::proto::pairing::{NONCE_LEN, confirm_tag, derive_secret};
use sunburst_core::proto::{ClientControl, DecoderQuirks, PairRequest, ServerControl};
use sunburst_net::ClientEndpoint;

use crate::pin::generate_pin;

/// Generate a PIN to display. The client shows it; the user types it into the
/// web UI. Returns an empty string only if the OS RNG fails.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_trexx_sunburst_PairActivity_nativeGenPin(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let pin = generate_pin().unwrap_or_default();
    to_jstring(env, &pin)
}

/// Run the pairing handshake against `host:port` with the shown `pin`. Returns
/// the 64-char hex secret on success, or an empty string on failure (the error
/// is logged). The server must be armed from the web UI first.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_trexx_sunburst_PairActivity_nativePair(
    mut env: JNIEnv,
    _class: JClass,
    host: JString,
    port: jint,
    pin: JString,
) -> jstring {
    let host: String = match env.get_string(&host) {
        Ok(s) => s.into(),
        Err(_) => return to_jstring(env, ""),
    };
    let pin: String = match env.get_string(&pin) {
        Ok(s) => s.into(),
        Err(_) => return to_jstring(env, ""),
    };
    match pair(&host, port as u16, &pin) {
        Ok(secret) => {
            let hex: String = secret.iter().map(|b| format!("{b:02x}")).collect();
            to_jstring(env, &hex)
        }
        Err(e) => {
            log::error!("pairing failed: {e}");
            to_jstring(env, "")
        }
    }
}

fn to_jstring(env: JNIEnv, s: &str) -> jstring {
    env.new_string(s)
        .map(|js| js.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// The handshake: PairRequest -> PairChallenge -> PairConfirm, deriving the
/// secret from the PIN and both nonces. Quirks are conservative here; the client
/// refreshes them from the decoder probe once streaming (a later commit).
fn pair(host: &str, port: u16, pin: &str) -> Result<[u8; 32], String> {
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

    // Await the challenge, ticking so the reliable PairRequest is retransmitted
    // while the user arms the web UI.
    let deadline = Instant::now() + Duration::from_secs(5);
    let server_nonce = loop {
        if let Some(ServerControl::PairChallenge { server_nonce, .. }) =
            client.recv_control().map_err(|e| e.to_string())?
        {
            break server_nonce;
        }
        client.tick().map_err(|e| e.to_string())?;
        if Instant::now() >= deadline {
            return Err("no challenge; arm pairing in the web UI first".into());
        }
    };

    let secret = derive_secret(pin, &client_nonce, &server_nonce);
    // The request id is not echoed to the client, so use 0 — the server matches
    // the confirm to its single pending request by nonce/tag, and this is a
    // single-device pairing session.
    client
        .send_control(&ClientControl::PairConfirm {
            request_id: 0,
            tag: confirm_tag(&secret),
        })
        .map_err(|e| e.to_string())?;

    // Keep answering briefly so the confirm is acknowledged rather than
    // retransmitted at an endpoint that has moved on.
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        let _ = client.recv_control();
        client.tick().map_err(|e| e.to_string())?;
    }
    Ok(secret)
}

fn android_model() -> String {
    // Best-effort; the model also travels the JVM but this avoids a JNI call.
    "Android TV".into()
}

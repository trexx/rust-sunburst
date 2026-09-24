// SPDX-License-Identifier: GPL-2.0-or-later
#![cfg(target_os = "android")]

//! The app grid's two calls, blocking, made from a Kotlin worker thread the way
//! `nativePair` is: fetch the catalogue (with box art), and launch an app.
//!
//! Both open a paired control connection **without `Hello`**: listing and
//! launching need no video session, and a `Hello` would start one. Each ends
//! with `Bye`, so the server forgets the connection at once and the stream that
//! follows (a fresh endpoint, a newer epoch) starts clean.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jni::JNIEnv;
use jni::objects::{JClass, JObject, JString};
use jni::sys::{jint, jobjectArray, jstring};
use sunburst_core::proto::{AppListing, ClientControl, ServerControl, SessionKey};
use sunburst_net::ClientEndpoint;

use crate::catalogue::{AppPages, ArtAssembler, ArtProgress, cache_name, stale};

/// How long to wait for the list, and for one image.
const LIST_TIMEOUT: Duration = Duration::from_secs(5);
const ART_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for a launch to be answered. A prep command (an HDR
/// toggle, a resolution change) runs before the answer.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);

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

/// A paired control connection, no `Hello`.
fn connect(host: &str, port: u16, secret_hex: &str) -> Result<ClientEndpoint, String> {
    let server: SocketAddr = format!("{host}:{port}")
        .parse()
        .map_err(|e| format!("{e}"))?;
    let secret = parse_secret(secret_hex).ok_or("no valid pairing secret; pair first")?;
    ClientEndpoint::connect(server, Some(SessionKey::from_bytes(secret))).map_err(|e| e.to_string())
}

/// Wait for a control message `pick` accepts, ticking so ours are resent.
fn await_control<T>(
    client: &mut ClientEndpoint,
    timeout: Duration,
    what: &str,
    mut pick: impl FnMut(ServerControl) -> Option<T>,
) -> Result<T, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(message) = client.recv_control().map_err(|e| e.to_string())?
            && let Some(found) = pick(message)
        {
            return Ok(found);
        }
        client.tick().map_err(|e| e.to_string())?;
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for {what}"));
        }
    }
}

/// The list, with each app's cached art path (fetching what is not cached,
/// one image at a time, and evicting what is no longer listed).
fn catalogue(
    host: &str,
    port: u16,
    secret_hex: &str,
    cache: &Path,
) -> Result<Vec<(AppListing, Option<PathBuf>)>, String> {
    let mut client = connect(host, port, secret_hex)?;
    client
        .send_control(&ClientControl::ListApps)
        .map_err(|e| e.to_string())?;
    let mut pages = AppPages::new();
    let apps = await_control(&mut client, LIST_TIMEOUT, "the app list", |m| {
        if let ServerControl::AppList(page) = m {
            pages.push(page);
        }
        pages.complete().map(<[AppListing]>::to_vec)
    })?;

    let _ = std::fs::create_dir_all(cache);
    let mut out = Vec::with_capacity(apps.len());
    for app in &apps {
        let path = match app.art {
            None => None,
            Some(r) => {
                let path = cache.join(cache_name(&r.digest, r.format));
                if path.exists() {
                    Some(path)
                } else {
                    fetch_art(&mut client, app.id, r.digest, cache)
                        .inspect_err(|e| log::warn!("art for app {}: {e}", app.id))
                        .ok()
                        .flatten()
                }
            }
        };
        out.push((app.clone(), path));
    }

    // Evict what no listing names any more.
    if let Ok(entries) = std::fs::read_dir(cache) {
        let names: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        for name in stale(&names, &apps) {
            let _ = std::fs::remove_file(cache.join(name));
        }
    }
    client.bye();
    Ok(out)
}

/// Fetch one image, verify it, and store it under its content name (written to
/// a temporary file and renamed, so a half-written image is never read).
fn fetch_art(
    client: &mut ClientEndpoint,
    app_id: u32,
    digest: [u8; 16],
    cache: &Path,
) -> Result<Option<PathBuf>, String> {
    client
        .send_control(&ClientControl::ArtRequest { app_id, digest })
        .map_err(|e| e.to_string())?;
    let mut assembler = ArtAssembler::new(app_id);
    let done = await_control(client, ART_TIMEOUT, "box art", |m| match m {
        ServerControl::ArtChunk(c) if c.app_id == app_id => match assembler.push(c) {
            Ok(ArtProgress::Partial) => None,
            Ok(ArtProgress::None) => Some(Ok(None)),
            Ok(ArtProgress::Done(digest, format, bytes)) => Some(Ok(Some((digest, format, bytes)))),
            Err(e) => Some(Err(format!("{e:?}"))),
        },
        _ => None,
    })??;
    let Some((digest, format, bytes)) = done else {
        return Ok(None);
    };
    let path = cache.join(cache_name(&digest, format));
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(Some(path))
}

fn launch(host: &str, port: u16, secret_hex: &str, app_id: u32) -> Result<(), String> {
    let mut client = connect(host, port, secret_hex)?;
    client
        .send_control(&ClientControl::LaunchApp { app_id })
        .map_err(|e| e.to_string())?;
    let result = await_control(&mut client, LAUNCH_TIMEOUT, "the launch", |m| match m {
        ServerControl::LaunchResult {
            app_id: id,
            ok,
            message,
        } if id == app_id => Some(if ok { Ok(()) } else { Err(message) }),
        _ => None,
    })?;
    client.bye();
    result
}

fn get_string(env: &mut JNIEnv, s: &JString) -> Option<String> {
    env.get_string(s).ok().map(Into::into)
}

/// The catalogue as a flat `String[]`: `id, name, artPath` per app (an empty
/// path for none). `null` on failure, logged.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_trexx_sunburst_LauncherActivity_nativeCatalogue(
    mut env: JNIEnv,
    _class: JClass,
    host: JString,
    port: jint,
    secret_hex: JString,
    cache_dir: JString,
) -> jobjectArray {
    let (Some(host), Some(secret), Some(cache)) = (
        get_string(&mut env, &host),
        get_string(&mut env, &secret_hex),
        get_string(&mut env, &cache_dir),
    ) else {
        return std::ptr::null_mut();
    };
    let apps = match catalogue(&host, port as u16, &secret, Path::new(&cache)) {
        Ok(apps) => apps,
        Err(e) => {
            log::error!("catalogue: {e}");
            return std::ptr::null_mut();
        }
    };
    let flat: Vec<String> = apps
        .iter()
        .flat_map(|(a, path)| {
            [
                a.id.to_string(),
                a.name.clone(),
                path.as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            ]
        })
        .collect();
    let Ok(array) = env.new_object_array(flat.len() as i32, "java/lang/String", JObject::null())
    else {
        return std::ptr::null_mut();
    };
    for (i, s) in flat.iter().enumerate() {
        let Ok(js) = env.new_string(s) else {
            return std::ptr::null_mut();
        };
        if env.set_object_array_element(&array, i as i32, js).is_err() {
            return std::ptr::null_mut();
        }
    }
    array.into_raw()
}

/// Launch an app. Returns "" on success, or why not.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_trexx_sunburst_LauncherActivity_nativeLaunch(
    mut env: JNIEnv,
    _class: JClass,
    host: JString,
    port: jint,
    secret_hex: JString,
    app_id: jint,
) -> jstring {
    let message = match (
        get_string(&mut env, &host),
        get_string(&mut env, &secret_hex),
    ) {
        (Some(host), Some(secret)) => match launch(&host, port as u16, &secret, app_id as u32) {
            Ok(()) => String::new(),
            Err(e) => e,
        },
        _ => "bad arguments".into(),
    };
    env.new_string(message)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

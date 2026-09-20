// SPDX-License-Identifier: GPL-2.0-or-later

//! Selecting the render endpoint to loopback-capture.
//!
//! The device is configurable: a name substring picks a specific endpoint (for
//! example "Steam Streaming Speakers", which silences the host while the client
//! still gets audio), and with no match — or no name — the system default render
//! endpoint is used, which always exists. Name matching degrades safely: an
//! endpoint whose friendly name cannot be read is skipped, not fatal, so a quirk
//! in one device never denies audio.

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize, STGM_READ,
};
use windows::core::Result;

/// Create the device enumerator. COM must already be initialised on this thread.
pub fn enumerator() -> Result<IMMDeviceEnumerator> {
    // SAFETY: standard COM activation of the MMDeviceEnumerator coclass.
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

/// The friendly name of an endpoint, or `None` if it cannot be read.
fn friendly_name(device: &IMMDevice) -> Option<String> {
    // SAFETY: opening the property store read-only and reading one string
    // value; the PROPVARIANT is a stack value we only read the `pwszVal` from
    // when the store returned success.
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let value = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        // Device names are VT_LPWSTR; `to_string` handles a null pointer by
        // erroring, which we map to `None`.
        value.Anonymous.Anonymous.Anonymous.pwszVal.to_string().ok()
    }
}

/// The friendly names of every active render endpoint.
///
/// Unlike [`select_render_endpoint`], which runs on the already-initialised
/// audio thread, this is called from the web thread where COM may not be
/// initialised, so it initialises COM (multithreaded) for the duration of the
/// walk and balances it on the way out — but only when it was the caller's
/// `CoInitializeEx` that succeeded, leaving an already-initialised apartment
/// alone. A failure anywhere yields an empty list rather than an error: the
/// device picker degrades to "type a name", it does not break the settings page.
pub fn list_render_endpoints() -> Vec<String> {
    // SAFETY: request the MTA. `Ok` means this call initialised COM and owns the
    // matching `CoUninitialize`; `RPC_E_CHANGED_MODE` (an `Err`) means the thread
    // was already an STA and we must not uninitialise it. Either way enumeration
    // works, so we only track whether to balance.
    let owned = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).is_ok() };
    let names = collect_render_endpoints().unwrap_or_default();
    if owned {
        // SAFETY: balances the successful CoInitializeEx above, on this thread.
        unsafe { CoUninitialize() };
    }
    names
}

/// Walk the active render endpoints, collecting the readable friendly names.
/// Split out so [`list_render_endpoints`] owns the COM lifetime and this owns the
/// `?`-propagation.
fn collect_render_endpoints() -> Result<Vec<String>> {
    let enumerator = enumerator()?;
    // SAFETY: enumerating active render endpoints and indexing the returned
    // collection by count, both standard IMMDeviceEnumerator calls.
    unsafe {
        let collection = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        let count = collection.GetCount()?;
        let mut names = Vec::with_capacity(count as usize);
        for i in 0..count {
            if let Some(name) = friendly_name(&collection.Item(i)?) {
                names.push(name);
            }
        }
        Ok(names)
    }
}

/// Find the active render endpoint whose friendly name contains `name_substr`
/// (case-insensitively), returning `None` if none matches — it never falls back
/// to the default endpoint.
///
/// This is the sink counterpart of [`select_render_endpoint`]: rendering the
/// pad-mic audio to the *wrong* endpoint (the speakers) would be worse than
/// silence, so the virtual-mic path (see [`crate::render`]) refuses to guess.
/// "Steam Streaming Microphone" is the intended target — a signed virtual mic
/// Steam installs — matched the same way "Steam Streaming Speakers" is on the
/// capture side.
pub fn find_render_endpoint(name_substr: &str) -> Result<Option<IMMDevice>> {
    let needle = name_substr.to_lowercase();
    if needle.is_empty() {
        return Ok(None);
    }
    let enumerator = enumerator()?;
    // SAFETY: enumerating active render endpoints and indexing the returned
    // collection by count, both standard IMMDeviceEnumerator calls.
    unsafe {
        let collection = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        let count = collection.GetCount()?;
        for i in 0..count {
            let device = collection.Item(i)?;
            if let Some(name) = friendly_name(&device)
                && name.to_lowercase().contains(&needle)
            {
                log::info!("audio: rendering pad mic to endpoint '{name}'");
                return Ok(Some(device));
            }
        }
    }
    log::warn!("audio: no render endpoint matched '{name_substr}'; pad mic will not be rendered");
    Ok(None)
}

/// Select the render endpoint to capture.
///
/// With `name_substr`, the first active render endpoint whose friendly name
/// contains it (case-insensitively) is returned; failing that, or with `None`,
/// the default console render endpoint.
pub fn select_render_endpoint(name_substr: Option<&str>) -> Result<IMMDevice> {
    let enumerator = enumerator()?;

    if let Some(needle) = name_substr.map(str::to_lowercase).filter(|s| !s.is_empty()) {
        // SAFETY: enumerating active render endpoints and indexing the returned
        // collection by count, both standard IMMDeviceEnumerator calls.
        unsafe {
            let collection = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
            let count = collection.GetCount()?;
            for i in 0..count {
                let device = collection.Item(i)?;
                if let Some(name) = friendly_name(&device)
                    && name.to_lowercase().contains(&needle)
                {
                    log::info!("audio: capturing from endpoint '{name}'");
                    return Ok(device);
                }
            }
        }
        log::warn!(
            "audio: no render endpoint matched '{needle}'; using the default endpoint (host audible)"
        );
    }

    // SAFETY: the default console render endpoint; present whenever any render
    // device is.
    unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
}

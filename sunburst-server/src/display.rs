// SPDX-License-Identifier: GPL-2.0-or-later

//! Native display control for the session: HDR (advanced color) and, opt-in,
//! resolution — with a guard that restores the prior state on drop.
//!
//! CLAUDE.md requires HDR to be toggled "around the session, and restore on
//! disconnect. Users will notice if you don't." That is what [`DisplayGuard`]
//! does: it snapshots the current advanced-color state of every active output
//! (and the primary display mode, if resolution matching is asked for), applies
//! the requested state, and restores exactly the snapshot on `Drop` — so an
//! abnormal session teardown (a panic, a killed process path) still leaves the
//! desktop as it was found.
//!
//! HDR toggles via `DisplayConfig{Get,Set}DeviceInfo` with the advanced-color
//! info/state packets; resolution via `ChangeDisplaySettingsExW`. Thin FFI,
//! box-validated (HARDWARE_TESTING §12) — the one pure, host-tested bit is the
//! millihertz→Hz rounding the client's `refresh_mhz` needs.

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DICS_DISABLE, DICS_ENABLE, DICS_FLAG_GLOBAL, DIF_PROPERTYCHANGE, DIGCF_ALLCLASSES,
    DIGCF_PRESENT, SP_CLASSINSTALL_HEADER, SP_DEVINFO_DATA, SP_PROPCHANGE_PARAMS, SPDRP_HARDWAREID,
    SetupDiCallClassInstaller, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo,
    SetupDiGetClassDevsW, SetupDiGetDeviceRegistryPropertyW, SetupDiSetClassInstallParamsW,
};
use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO, DISPLAYCONFIG_DEVICE_INFO_HEADER,
    DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE, DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE,
    DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo, GetDisplayConfigBufferSizes,
    QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
};
use windows::Win32::Foundation::{ERROR_SUCCESS, LUID};
use windows::Win32::Graphics::Gdi::{
    CDS_TEST, CDS_UPDATEREGISTRY, ChangeDisplaySettingsExW, DEVMODE_FIELD_FLAGS, DEVMODEW,
    DISP_CHANGE_SUCCESSFUL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH,
    ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    SPI_GETMOUSE, SPI_GETMOUSESPEED, SPI_SETMOUSE, SPI_SETMOUSESPEED, SPIF_SENDCHANGE,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
};
use windows::core::PCWSTR;

/// Convert the client's `refresh_mhz` (millihertz) to the integer Hz a display
/// mode carries, rounding to nearest. Pure, so it is host-tested.
pub fn refresh_hz(refresh_mhz: u32) -> u32 {
    (refresh_mhz + 500) / 1000
}

/// One active output: the adapter LUID and target id a display-config packet
/// addresses.
#[derive(Clone, Copy)]
struct Target {
    adapter: LUID,
    id: u32,
}

/// Enumerate the active display paths, returning their output targets.
fn active_targets() -> Vec<Target> {
    let mut num_paths = 0u32;
    let mut num_modes = 0u32;
    // SAFETY: out-params for the buffer sizes; QDC_ONLY_ACTIVE_PATHS is a valid flag.
    let sized = unsafe {
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut num_paths, &mut num_modes)
    };
    if sized != ERROR_SUCCESS || num_paths == 0 {
        return Vec::new();
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); num_paths as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); num_modes as usize];
    // SAFETY: buffers are sized to the counts just queried; the counts are
    // updated in place to what was written.
    let queried = unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut num_paths,
            paths.as_mut_ptr(),
            &mut num_modes,
            modes.as_mut_ptr(),
            None,
        )
    };
    if queried != ERROR_SUCCESS {
        return Vec::new();
    }
    paths
        .iter()
        .take(num_paths as usize)
        .map(|p| Target {
            adapter: p.targetInfo.adapterId,
            id: p.targetInfo.id,
        })
        .collect()
}

/// `(supported, enabled)` advanced-color state for one target, or `None` if the
/// query failed.
fn advanced_color(target: Target) -> Option<(bool, bool)> {
    let mut info = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
            size: size_of::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>() as u32,
            adapterId: target.adapter,
            id: target.id,
        },
        ..Default::default()
    };
    // SAFETY: `info.header` is a correctly-typed and -sized request packet.
    let rc = unsafe { DisplayConfigGetDeviceInfo(&mut info.header) };
    if rc != ERROR_SUCCESS.0 as i32 {
        return None;
    }
    // SAFETY: reading the union as its raw bitfield value. bit0 = supported,
    // bit1 = enabled.
    let bits = unsafe { info.Anonymous.value };
    Some((bits & 0x1 != 0, bits & 0x2 != 0))
}

/// Enable or disable advanced color on one target. Returns whether it succeeded.
fn set_advanced_color(target: Target, enable: bool) -> bool {
    let mut state = DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE,
            size: size_of::<DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE>() as u32,
            adapterId: target.adapter,
            id: target.id,
        },
        ..Default::default()
    };
    // bit0 = enableAdvancedColor.
    state.Anonymous.value = u32::from(enable);
    // SAFETY: `state.header` is a correctly-typed and -sized set packet.
    unsafe { DisplayConfigSetDeviceInfo(&state.header) == ERROR_SUCCESS.0 as i32 }
}

/// The current mode of the primary display, for later restore.
fn current_mode() -> Option<DEVMODEW> {
    let mut mode = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: null device name = the default display; `mode` is a valid out-param.
    let ok = unsafe { EnumDisplaySettingsW(PCWSTR::null(), ENUM_CURRENT_SETTINGS, &mut mode) };
    ok.as_bool().then_some(mode)
}

/// Apply a display mode to the primary display, `CDS_TEST`-validated first so an
/// unsupported request is refused rather than blanking the screen. Returns
/// whether it applied.
fn apply_mode(mode: &DEVMODEW) -> bool {
    // SAFETY: `mode` is a fully-populated DEVMODEW; null device name = primary.
    unsafe {
        if ChangeDisplaySettingsExW(PCWSTR::null(), Some(mode), None, CDS_TEST, None)
            != DISP_CHANGE_SUCCESSFUL
        {
            return false;
        }
        ChangeDisplaySettingsExW(PCWSTR::null(), Some(mode), None, CDS_UPDATEREGISTRY, None)
            == DISP_CHANGE_SUCCESSFUL
    }
}

/// Restores the display state it was built with when dropped.
///
/// Held in the session's `Active`; dropping it (session stop, or an abnormal
/// teardown) puts advanced color and the display mode back exactly as found.
pub struct DisplayGuard {
    /// `(target, was_enabled)` for every target whose advanced color we changed.
    hdr_restore: Vec<(Target, bool)>,
    /// The mode to restore, if resolution matching changed it.
    mode_restore: Option<DEVMODEW>,
}

impl DisplayGuard {
    /// Set the display up for a session: enable HDR on every capable output when
    /// `hdr`, and switch the primary display to `resolution` when asked. Both
    /// are snapshotted for restore. A no-op guard (`hdr` false, `resolution`
    /// none) still restores nothing, harmlessly.
    pub fn apply(hdr: bool, resolution: Option<(u32, u32, u32)>) -> DisplayGuard {
        let mut hdr_restore = Vec::new();
        if hdr {
            for target in active_targets() {
                if let Some((supported, enabled)) = advanced_color(target)
                    && supported
                    && !enabled
                    && set_advanced_color(target, true)
                {
                    hdr_restore.push((target, enabled));
                }
            }
        }

        let mut mode_restore = None;
        if let Some((w, h, hz)) = resolution {
            let prior = current_mode();
            let mut mode = prior.unwrap_or(DEVMODEW {
                dmSize: size_of::<DEVMODEW>() as u16,
                ..Default::default()
            });
            mode.dmPelsWidth = w;
            mode.dmPelsHeight = h;
            mode.dmDisplayFrequency = hz;
            mode.dmFields |=
                DEVMODE_FIELD_FLAGS(DM_PELSWIDTH.0 | DM_PELSHEIGHT.0 | DM_DISPLAYFREQUENCY.0);
            if apply_mode(&mode) {
                mode_restore = prior;
            }
        }

        DisplayGuard {
            hdr_restore,
            mode_restore,
        }
    }
}

impl Drop for DisplayGuard {
    fn drop(&mut self) {
        for (target, was_enabled) in &self.hdr_restore {
            set_advanced_color(*target, *was_enabled);
        }
        if let Some(mode) = &self.mode_restore {
            apply_mode(mode);
        }
    }
}

/// Hardware-ID substrings that identify a consumable virtual display driver.
/// The MikeTheTech "Virtual Display Driver" has used several across versions;
/// matched case-insensitively. Box-tuned — a new VDD build may need another.
const VDD_HARDWARE_IDS: &[&str] = &["mttvdd", "virtualdisplaydriver", "iddsampledriver"];

/// The first string of a `REG_MULTI_SZ`/`REG_SZ` UTF-16 property buffer.
fn first_utf16_string(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// Enable or disable every present device whose hardware id matches a known
/// virtual-display driver. Returns whether any matched. Requires the elevated
/// server (device state changes need admin — which the scheduled task grants).
fn set_vdd_state(enable: bool) -> bool {
    // SAFETY: enumerate all present devices; the returned set is destroyed below.
    let hdevinfo = match unsafe {
        SetupDiGetClassDevsW(None, PCWSTR::null(), None, DIGCF_ALLCLASSES | DIGCF_PRESENT)
    } {
        Ok(h) => h,
        Err(_) => return false,
    };

    let mut found = false;
    let mut index = 0u32;
    loop {
        let mut data = SP_DEVINFO_DATA {
            cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        // SAFETY: `data` is a correctly-sized out-param; an Err means the
        // enumeration is exhausted.
        if unsafe { SetupDiEnumDeviceInfo(hdevinfo, index, &mut data) }.is_err() {
            break;
        }
        index += 1;

        let mut buf = [0u8; 512];
        // SAFETY: reading the hardware-id property into `buf`; a failure just
        // means this device has none we can read, so skip it.
        if unsafe {
            SetupDiGetDeviceRegistryPropertyW(
                hdevinfo,
                &data,
                SPDRP_HARDWAREID,
                None,
                Some(&mut buf),
                None,
            )
        }
        .is_err()
        {
            continue;
        }
        let hwid = first_utf16_string(&buf).to_lowercase();
        if !VDD_HARDWARE_IDS.iter().any(|m| hwid.contains(m)) {
            continue;
        }

        let params = SP_PROPCHANGE_PARAMS {
            ClassInstallHeader: SP_CLASSINSTALL_HEADER {
                cbSize: size_of::<SP_CLASSINSTALL_HEADER>() as u32,
                InstallFunction: DIF_PROPERTYCHANGE,
            },
            StateChange: if enable { DICS_ENABLE } else { DICS_DISABLE },
            Scope: DICS_FLAG_GLOBAL,
            HwProfile: 0,
        };
        // SAFETY: the propchange params start with the class-install header;
        // passing its address with the full struct size is the documented call.
        unsafe {
            let _ = SetupDiSetClassInstallParamsW(
                hdevinfo,
                Some(&data),
                Some(&params.ClassInstallHeader),
                size_of::<SP_PROPCHANGE_PARAMS>() as u32,
            );
            let _ = SetupDiCallClassInstaller(DIF_PROPERTYCHANGE, hdevinfo, Some(&data));
        }
        found = true;
    }

    // SAFETY: `hdevinfo` came from SetupDiGetClassDevsW and is freed once.
    unsafe {
        let _ = SetupDiDestroyDeviceInfoList(hdevinfo);
    }
    found
}

/// Enables a consumable virtual display for a session and disables it on drop.
///
/// Consumes an already-installed VDD (the MikeTheTech driver) rather than
/// authoring one — device state changes only. [`enable`](Self::enable) returns
/// `None` when no such driver is present, so the caller falls back to the
/// physical display.
pub struct VirtualDisplay;

impl VirtualDisplay {
    /// Enable the virtual display, or `None` if no VDD is installed.
    pub fn enable() -> Option<VirtualDisplay> {
        set_vdd_state(true).then_some(VirtualDisplay)
    }
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        set_vdd_state(false);
    }
}

/// Turns Enhanced Pointer Precision (mouse acceleration) off for a session,
/// pins the pointer speed to 1:1, and restores both on drop.
///
/// EPP is the third value of the system `MOUSE` parameters (the acceleration
/// flag); `SPI_SETMOUSE` with it zeroed disables the acceleration curve that
/// CLAUDE.md warns makes injected relative deltas feel wrong. The pointer speed
/// (`SPI_SETMOUSESPEED`, 1–20) is pinned to 10, which with EPP off is exactly
/// 1:1: one injected count is one pixel, so the only gain between the client's
/// mouse and the server's pointer is our own sensitivity, and a client
/// predicting its cursor multiplies by exactly that. Opt-in (`disable_epp`);
/// otherwise the OS settings are left exactly as the user has them.
pub struct EppGuard {
    /// The `[threshold1, threshold2, acceleration]` to restore, if we changed it.
    restore_mouse: Option<[i32; 3]>,
    /// The pointer speed to restore, if we changed it.
    restore_speed: Option<u32>,
}

/// The pointer speed at which Windows moves one pixel per count (EPP off).
const ONE_TO_ONE_SPEED: u32 = 10;

fn get_mouse() -> Option<[i32; 3]> {
    let mut params = [0i32; 3];
    // SAFETY: SPI_GETMOUSE fills a 3-element i32 array via `pvparam`.
    unsafe {
        SystemParametersInfoW(
            SPI_GETMOUSE,
            0,
            Some(params.as_mut_ptr().cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .ok()
    .map(|()| params)
}

fn set_mouse(mut params: [i32; 3]) {
    // SAFETY: SPI_SETMOUSE reads the 3-element array; SENDCHANGE notifies apps.
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_SETMOUSE,
            0,
            Some(params.as_mut_ptr().cast()),
            SPIF_SENDCHANGE,
        );
    }
}

fn get_speed() -> Option<u32> {
    let mut speed = 0u32;
    // SAFETY: SPI_GETMOUSESPEED writes one integer through `pvparam`.
    unsafe {
        SystemParametersInfoW(
            SPI_GETMOUSESPEED,
            0,
            Some((&mut speed as *mut u32).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .ok()
    .map(|()| speed)
}

fn set_speed(speed: u32) {
    // SAFETY: SPI_SETMOUSESPEED takes the value itself in `pvparam`, not a
    // pointer to it; nothing is dereferenced.
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_SETMOUSESPEED,
            0,
            Some(speed as usize as *mut core::ffi::c_void),
            SPIF_SENDCHANGE,
        );
    }
}

/// The pointer's current `(acceleration on, speed)`, as the gain reported to
/// the client is computed from. `None` if either cannot be read.
pub fn pointer_state() -> Option<(bool, u32)> {
    Some((get_mouse()?[2] != 0, get_speed()?))
}

impl EppGuard {
    /// Disable EPP and pin the speed now (each only if not already so),
    /// returning a guard that restores whatever it changed on drop.
    pub fn disable() -> EppGuard {
        let restore_mouse = get_mouse().filter(|p| p[2] != 0).inspect(|params| {
            let mut off = *params;
            off[2] = 0;
            set_mouse(off);
        });
        let restore_speed = get_speed()
            .filter(|&s| s != ONE_TO_ONE_SPEED)
            .inspect(|_| set_speed(ONE_TO_ONE_SPEED));
        EppGuard {
            restore_mouse,
            restore_speed,
        }
    }
}

impl Drop for EppGuard {
    fn drop(&mut self) {
        if let Some(params) = self.restore_mouse {
            set_mouse(params);
        }
        if let Some(speed) = self.restore_speed {
            set_speed(speed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_utf16_string_stops_at_the_nul() {
        // "AB" as UTF-16LE, then a terminator and trailing garbage.
        let bytes = [b'A', 0, b'B', 0, 0, 0, b'X', 0];
        assert_eq!(first_utf16_string(&bytes), "AB");
    }

    #[test]
    fn refresh_rounds_to_nearest_hz() {
        assert_eq!(refresh_hz(59_940), 60); // 59.94 Hz
        assert_eq!(refresh_hz(60_000), 60);
        assert_eq!(refresh_hz(120_000), 120);
        assert_eq!(refresh_hz(143_900), 144);
        assert_eq!(refresh_hz(0), 0);
    }
}

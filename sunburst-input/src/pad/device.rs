// SPDX-License-Identifier: GPL-2.0-or-later

//! Create the per-controller virtual device node the driver binds to.
//!
//! [`install`](super::install) makes the driver available machine-wide; this
//! makes one pad. It ports HIDMaestro's `DeviceNodeCreator` — three enumerator
//! paths picked from the profile:
//!
//! - **Plain HID** (DualSense, DualShock 4, Switch Pro): a ROOT node in the HID
//!   class with hardware ID `root\VID_xxxx&PID_yyyy`, which `hidmaestro.inf`
//!   matches. No companion.
//! - **xinputhid** (Xbox Series, `driverMode: xinputhid`): the main node carries
//!   `&IG_00` + `xinputhid` in `UpperFilters`, and a SWD-enumerated **gamepad
//!   companion** shares its ContainerId so Windows presents one XInput device.
//! - **Xbox legacy** (Xbox 360 wired): the main node carries `&IG_00` +
//!   `USB\MS_COMP_XUSB10` compatible IDs, and a SWD-enumerated **XUSB companion**
//!   (bound by `hidmaestro_xusb.inf`, driven by `HMXInput.dll`) presents the
//!   XInput device.
//!
//! **`ControllerIndex` is the seam to [`shmem`](super::shmem):** written on the
//! main node (and, for the XUSB path, on the companion) so the driver opens the
//! matching `Global\HIDMaestro*{N}` sections.
//!
//! **`SwDeviceCreate` directly.** HIDMaestro's C# shells out to `hmswd.exe` only
//! to dodge a .NET 10 marshaling bug; Rust calls `cfgmgr32!SwDeviceCreate` in
//! process. The companion uses the default `Handle` lifetime, so closing the
//! returned `HSWDEVICE` removes it — no resurrect dance.
//!
//! **Admin.** `DIF_REGISTERDEVICE` / `SwDeviceCreate` need elevation; a
//! non-elevated caller fails here.
//!
//! Windows-only, gated at the module in `pad/mod.rs`.

use std::ffi::c_void;
use std::fmt;

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DIF_REGISTERDEVICE, DIF_REMOVE, HDEVINFO, SETUP_DI_DEVICE_CREATION_FLAGS, SP_DEVINFO_DATA,
    SPDRP_COMPATIBLEIDS, SPDRP_HARDWAREID, SPDRP_UPPERFILTERS, SetupDiCallClassInstaller,
    SetupDiCreateDeviceInfoList, SetupDiCreateDeviceInfoW, SetupDiDestroyDeviceInfoList,
    SetupDiSetDeviceRegistryPropertyW, UPDATEDRIVERFORPLUGANDPLAYDEVICES_FLAGS,
    UpdateDriverForPlugAndPlayDevicesW,
};
use windows::Win32::Devices::Enumeration::Pnp::{
    HSWDEVICE, SW_DEVICE_CREATE_INFO, SWDeviceCapabilitiesDriverRequired,
    SWDeviceCapabilitiesRemovable, SwDeviceClose, SwDeviceCreate,
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, GetLastError, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE, RegCloseKey,
    RegCreateKeyExW, RegSetValueExW,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::core::{GUID, HRESULT, HSTRING, PCWSTR};

use super::profile::Profile;

/// `{745a17a0-74d3-11d0-b6fe-00a0c90f57da}` — the HID device class every virtual
/// controller (companions aside) is created in.
const HID_CLASS_GUID: GUID = GUID::from_values(
    0x745a_17a0,
    0x74d3,
    0x11d0,
    [0xb6, 0xfe, 0x00, 0xa0, 0xc9, 0x0f, 0x57, 0xda],
);

/// How long to wait for `SwDeviceCreate`'s callback (driver bind is synchronous
/// under `DriverRequired`, but PnP under load can serialise).
const COMPANION_TIMEOUT_MS: u32 = 15_000;

/// Which enumerator path a pad takes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    PlainHid,
    Xinputhid,
    XboxLegacy,
}

/// What a pad node needs: identity for the hardware IDs and the section index.
#[derive(Debug, Clone)]
pub struct PadNodeSpec {
    /// USB vendor id.
    pub vid: u16,
    /// USB product id — what apps read via HID attributes.
    pub pid: u16,
    /// PID used to form the hardware ID the driver INF matches (xinputhid's INF
    /// wants `0x02FF`); equals `pid` when the profile does not override it.
    pub driver_pid: u16,
    /// The pad routes through xinputhid / xusb22 (Xbox Series).
    pub uses_upper_filter: bool,
    /// The pad needs the XUSB companion (Xbox 360 family).
    pub requires_xusb_companion: bool,
    /// Human-readable device description (shows in Device Manager).
    pub description: String,
    /// Which `Global\HIDMaestro*{index}` section set this pad drives.
    pub controller_index: u8,
}

impl PadNodeSpec {
    /// Build a spec from a parsed profile. Requires VID + PID.
    pub fn from_profile(profile: &Profile, controller_index: u8) -> Option<PadNodeSpec> {
        Some(PadNodeSpec {
            vid: profile.vid_u16()?,
            pid: profile.pid_u16()?,
            driver_pid: profile.driver_hw_pid()?,
            uses_upper_filter: profile.uses_upper_filter(),
            requires_xusb_companion: profile.requires_xusb_companion(),
            description: profile.display_name().to_string(),
            controller_index,
        })
    }

    fn kind(&self) -> Kind {
        if self.uses_upper_filter {
            Kind::Xinputhid
        } else if self.requires_xusb_companion {
            Kind::XboxLegacy
        } else {
            Kind::PlainHid
        }
    }

    /// `HM_{index:04}` — the deterministic instance-name segment, stable across
    /// the server's lives so the HID child keeps its path.
    fn token(&self) -> String {
        format!("HM_{:04}", self.controller_index)
    }

    /// The primary hardware ID the main node's INF matches on.
    fn hardware_id(&self) -> String {
        match self.kind() {
            Kind::PlainHid => format!("root\\VID_{:04X}&PID_{:04X}", self.vid, self.pid),
            Kind::Xinputhid => {
                format!(
                    "root\\VID_{:04X}&PID_{:04X}&IG_00",
                    self.vid, self.driver_pid
                )
            }
            Kind::XboxLegacy => format!("root\\VID_{:04X}&PID_{:04X}&IG_00", self.vid, self.pid),
        }
    }

    /// The full instance id: `ROOT\<enumerator>\<token>`.
    fn instance_id(&self) -> String {
        let enumerator = match self.kind() {
            Kind::PlainHid => "HIDClass".to_string(),
            Kind::Xinputhid => format!("VID_{:04X}&PID_{:04X}&IG_00", self.vid, self.driver_pid),
            Kind::XboxLegacy => format!("VID_{:04X}&PID_{:04X}&IG_00", self.vid, self.pid),
        };
        format!("ROOT\\{enumerator}\\{}", self.token())
    }
}

/// Per-controller ContainerId — ASCII "HIDMAESTRO" + the 16-bit index — shared by
/// the main node and its companion so Windows groups them as one controller.
/// Mirrors HIDMaestro's `SwdDeviceFactory.ContainerIdFor`.
fn container_id_for(index: u8) -> GUID {
    GUID::from_values(
        0x4849_4430,
        0x4D41,
        0x4553,
        [0x54, 0x52, 0x4F, 0x00, 0x00, 0x00, 0x00, index],
    )
}

/// Why creating a pad node failed.
#[derive(Debug)]
pub enum DeviceError {
    /// Device registration needs elevation; the server is not elevated.
    AccessDenied,
    /// A SetupAPI / CfgMgr / SwDevice call failed; carries a Win32 message.
    Win32(String),
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeviceError::AccessDenied => {
                write!(f, "device registration denied — run the server elevated")
            }
            DeviceError::Win32(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for DeviceError {}

/// A live virtual pad. Dropping it removes the main node and any companion.
pub struct PadNode {
    dev_info: HDEVINFO,
    data: SP_DEVINFO_DATA,
    instance_id: String,
    /// The SWD companion (Xbox paths), whose `Handle` lifetime ends on close.
    companion: Option<HSWDEVICE>,
    removed: bool,
}

/// Create the virtual pad node for `spec` and bind the driver at `inf_path`.
pub fn create(spec: &PadNodeSpec, inf_path: &std::path::Path) -> Result<PadNode, DeviceError> {
    let kind = spec.kind();
    let instance_id = spec.instance_id();
    let hardware_id = spec.hardware_id();
    let hw_multi = multi_sz(&[&hardware_id, "root\\HIDMaestro"]);

    // SAFETY: null class-image list is valid; a -1 HDEVINFO is the failure
    // sentinel, which the `windows` Result maps to Err.
    let dev_info = unsafe { SetupDiCreateDeviceInfoList(Some(&HID_CLASS_GUID), None) }
        .map_err(|e| DeviceError::Win32(format!("SetupDiCreateDeviceInfoList: {e}")))?;

    let mut node = PadNode {
        dev_info,
        data: SP_DEVINFO_DATA {
            cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        },
        instance_id: instance_id.clone(),
        companion: None,
        removed: false,
    };

    let inst_w = HSTRING::from(&instance_id);
    let desc_w = HSTRING::from(&spec.description);

    // Without DICD_GENERATE_ID (flags 0) the name is the full instance id.
    // SAFETY: `dev_info` is live; the strings are NUL-terminated; `data` has
    // cbSize set and is written in place.
    unsafe {
        SetupDiCreateDeviceInfoW(
            node.dev_info,
            PCWSTR(inst_w.as_ptr()),
            &HID_CLASS_GUID,
            PCWSTR(desc_w.as_ptr()),
            None,
            SETUP_DI_DEVICE_CREATION_FLAGS(0),
            Some(&mut node.data),
        )
    }
    .map_err(|e| DeviceError::Win32(format!("SetupDiCreateDeviceInfoW({instance_id}): {e}")))?;

    set_property(&mut node, SPDRP_HARDWAREID, &hw_multi, "HARDWAREID")?;

    // These MUST be set before DIF_REGISTERDEVICE: xinputhid's UpperFilters check
    // and WGI's compatible-id read both run at the node's first registration
    // (issue #59) — a post-registration write is silently ignored.
    match kind {
        Kind::Xinputhid => {
            let uf = multi_sz(&["xinputhid"]);
            set_property(&mut node, SPDRP_UPPERFILTERS, &uf, "UPPERFILTERS")?;
        }
        Kind::XboxLegacy => {
            let compat = multi_sz(&[
                "USB\\MS_COMP_XUSB10",
                "USB\\Class_FF&SubClass_5D&Prot_01",
                "USB\\Class_FF&SubClass_5D",
                "USB\\Class_FF",
            ]);
            set_property(&mut node, SPDRP_COMPATIBLEIDS, &compat, "COMPATIBLEIDS")?;
        }
        Kind::PlainHid => {}
    }

    // DIF_REGISTERDEVICE creates the PnP node — admin-only.
    // SAFETY: `dev_info`/`data` are live and registered together.
    unsafe { SetupDiCallClassInstaller(DIF_REGISTERDEVICE, node.dev_info, Some(&node.data)) }
        .map_err(|_| register_error())?;

    // The node exists now — from here, drop must also DIF_REMOVE it.
    write_controller_index(&instance_id, spec.controller_index)?;
    bind_driver(&hardware_id, inf_path)?;

    // The Xbox paths need their SWD companion; a failure here removes the main
    // node too (via `node`'s Drop on the `?`).
    match kind {
        Kind::Xinputhid => node.companion = Some(create_gamepad_companion(spec)?),
        Kind::XboxLegacy => node.companion = Some(create_xusb_companion(spec)?),
        Kind::PlainHid => {}
    }

    Ok(node)
}

/// The gamepad companion for the xinputhid (Xbox Series) path.
fn create_gamepad_companion(spec: &PadNodeSpec) -> Result<HSWDEVICE, DeviceError> {
    let vid_hw = format!(
        "root\\VID_{:04X}&PID_{:04X}&IG_00",
        spec.vid, spec.driver_pid
    );
    let enumerator = format!(
        "HIDMAESTRO_VID_{:04X}_PID_{:04X}&IG_00",
        spec.vid, spec.driver_pid
    );
    companion(
        &enumerator,
        spec,
        &[&vid_hw, "root\\HIDMaestroGamepad", "root\\HIDMaestro"],
        &["root\\HIDMaestroGamepad", "root\\HIDMaestro"],
        false,
    )
}

/// The XUSB companion for the Xbox 360 legacy path — bound by `hidmaestro_xusb.inf`.
fn create_xusb_companion(spec: &PadNodeSpec) -> Result<HSWDEVICE, DeviceError> {
    let vid_hw = format!("root\\VID_{:04X}&PID_{:04X}&XI_00", spec.vid, spec.pid);
    companion(
        "HIDMAESTRO",
        spec,
        &[&vid_hw, "root\\HIDMaestroXUSB"],
        &[
            "USB\\MS_COMP_XUSB10",
            "USB\\Class_FF&SubClass_5D&Prot_01",
            "USB\\Class_FF&SubClass_5D",
            "USB\\Class_FF",
        ],
        // The XUSB companion driver (HMXInput.dll) reads ControllerIndex too.
        true,
    )
}

/// Context the `SwDeviceCreate` callback fills in. Boxed and passed by raw
/// pointer; reclaimed on success, leaked on timeout (a late callback would
/// otherwise write to freed memory).
struct CompanionCtx {
    event: HANDLE,
    result: HRESULT,
    instance_id: Option<String>,
}

/// The `SwDeviceCreate` completion callback: record the result + instance id and
/// signal the waiting thread.
unsafe extern "system" fn companion_callback(
    _hsw: HSWDEVICE,
    create_result: HRESULT,
    context: *const c_void,
    instance_id: PCWSTR,
) {
    if context.is_null() {
        return;
    }
    // SAFETY: `context` is the boxed `CompanionCtx` this create passed, alive
    // until the waiter reclaims it (success) or forever (timeout leak).
    let ctx = unsafe { &mut *(context as *mut CompanionCtx) };
    ctx.result = create_result;
    if !instance_id.is_null() {
        // SAFETY: a NUL-terminated wide string valid for this callback.
        ctx.instance_id = unsafe { instance_id.to_string() }.ok();
    }
    // SAFETY: `ctx.event` is a live manual-reset event.
    unsafe {
        let _ = SetEvent(ctx.event);
    }
}

/// Create an SWD-enumerated companion device and wait for its bind.
fn companion(
    enumerator: &str,
    spec: &PadNodeSpec,
    hardware_ids: &[&str],
    compat_ids: &[&str],
    write_index: bool,
) -> Result<HSWDEVICE, DeviceError> {
    let hw = multi_sz(hardware_ids);
    let compat = multi_sz(compat_ids);
    let container = container_id_for(spec.controller_index);
    let enum_w = HSTRING::from(enumerator);
    let parent_w = HSTRING::from("HTREE\\ROOT\\0");
    let suffix_w = HSTRING::from(&spec.token());
    let desc_w = HSTRING::from(&spec.description);

    let info = SW_DEVICE_CREATE_INFO {
        cbSize: size_of::<SW_DEVICE_CREATE_INFO>() as u32,
        pszInstanceId: PCWSTR(suffix_w.as_ptr()),
        pszzHardwareIds: PCWSTR(hw.as_ptr()),
        pszzCompatibleIds: PCWSTR(compat.as_ptr()),
        pContainerId: &container,
        CapabilityFlags: (SWDeviceCapabilitiesDriverRequired.0 | SWDeviceCapabilitiesRemovable.0)
            as u32,
        pszDeviceDescription: PCWSTR(desc_w.as_ptr()),
        ..Default::default()
    };

    // Manual-reset, initially unsignalled.
    // SAFETY: a null name / default attrs are valid.
    let event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
        .map_err(|e| DeviceError::Win32(format!("CreateEventW: {e}")))?;

    let ctx = Box::into_raw(Box::new(CompanionCtx {
        event,
        result: HRESULT(0),
        instance_id: None,
    }));

    // SAFETY: all pointers in `info` outlive the call; `ctx` is a live boxed
    // context; the callback matches SW_DEVICE_CREATE_CALLBACK.
    let created = unsafe {
        SwDeviceCreate(
            PCWSTR(enum_w.as_ptr()),
            PCWSTR(parent_w.as_ptr()),
            &info,
            None,
            Some(companion_callback),
            Some(ctx as *const c_void),
        )
    };
    let hsw = match created {
        Ok(h) => h,
        Err(e) => {
            // The callback never fired, so reclaiming `ctx` is safe.
            // SAFETY: `ctx` came from Box::into_raw and is dropped once; `event`
            // is live.
            unsafe {
                drop(Box::from_raw(ctx));
                let _ = CloseHandle(event);
            }
            return Err(DeviceError::Win32(format!("SwDeviceCreate: {e}")));
        }
    };

    // SAFETY: `event` is a live event handle.
    let wait = unsafe { WaitForSingleObject(event, COMPANION_TIMEOUT_MS) };
    if wait != WAIT_OBJECT_0 {
        // Leak `ctx`: a late callback may still write to it. Bounded — an
        // error path only.
        // SAFETY: abandon the half-created device and close our event.
        unsafe {
            SwDeviceClose(hsw);
            let _ = CloseHandle(event);
        }
        return Err(DeviceError::Win32(
            "SwDeviceCreate callback timed out".into(),
        ));
    }

    // Signalled: the callback has run and will not touch `ctx` again — reclaim.
    // SAFETY: `ctx` is the live box, reclaimed exactly once here.
    let ctx = unsafe { Box::from_raw(ctx) };
    // SAFETY: `event` is live and closed once.
    unsafe {
        let _ = CloseHandle(event);
    }

    if ctx.result.is_err() {
        // SAFETY: the created handle is closed once.
        unsafe { SwDeviceClose(hsw) };
        return Err(DeviceError::Win32(format!(
            "SwDeviceCreate failed: {}",
            ctx.result.message()
        )));
    }

    if write_index && let Some(id) = &ctx.instance_id {
        // Best effort — the companion still exists if this fails.
        let _ = write_controller_index(id, spec.controller_index);
    }
    Ok(hsw)
}

impl PadNode {
    /// The main device instance id.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Remove the device (and companion) now, rather than at drop. Idempotent.
    pub fn remove(&mut self) -> Result<(), DeviceError> {
        if self.removed {
            return Ok(());
        }
        self.removed = true;
        if let Some(companion) = self.companion.take() {
            // SAFETY: our handle, closed once — its Handle lifetime removes the
            // companion device.
            unsafe { SwDeviceClose(companion) };
        }
        // SAFETY: `dev_info`/`data` describe the node this removes.
        let r = unsafe { SetupDiCallClassInstaller(DIF_REMOVE, self.dev_info, Some(&self.data)) };
        r.map_err(|e| DeviceError::Win32(format!("DIF_REMOVE({}): {e}", self.instance_id)))
    }
}

impl Drop for PadNode {
    fn drop(&mut self) {
        let _ = self.remove();
        // SAFETY: `dev_info` came from SetupDiCreateDeviceInfoList; freed once.
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.dev_info);
        }
    }
}

/// Set a REG_MULTI_SZ device registry property on the in-progress node.
fn set_property(
    node: &mut PadNode,
    property: windows::Win32::Devices::DeviceAndDriverInstallation::SETUP_DI_REGISTRY_PROPERTY,
    value: &[u16],
    what: &str,
) -> Result<(), DeviceError> {
    // SAFETY: `dev_info`/`data` are live; `value` is a REG_MULTI_SZ of u16.
    unsafe {
        SetupDiSetDeviceRegistryPropertyW(
            node.dev_info,
            &mut node.data,
            property,
            Some(as_bytes(value)),
        )
    }
    .map_err(|e| DeviceError::Win32(format!("set {what}: {e}")))
}

/// Map a `DIF_REGISTERDEVICE` failure to AccessDenied when it was elevation.
fn register_error() -> DeviceError {
    // SAFETY: GetLastError just reads the calling thread's last error.
    let err = unsafe { GetLastError() };
    if err == ERROR_ACCESS_DENIED {
        DeviceError::AccessDenied
    } else {
        DeviceError::Win32(format!("DIF_REGISTERDEVICE failed (Win32 {})", err.0))
    }
}

/// Write `HKLM\…\Enum\{inst}\Device Parameters\ControllerIndex` — the value the
/// driver reads to pick its shared section.
fn write_controller_index(instance_id: &str, index: u8) -> Result<(), DeviceError> {
    let sub = format!("SYSTEM\\CurrentControlSet\\Enum\\{instance_id}\\Device Parameters");
    let sub_w = HSTRING::from(&sub);
    let name_w = HSTRING::from("ControllerIndex");
    let value = u32::from(index);

    let mut hkey = HKEY::default();
    // SAFETY: HKLM is predefined; out-param `hkey` is written on success.
    let rc = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(sub_w.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut hkey,
            None,
        )
    };
    if rc.is_err() {
        return Err(DeviceError::Win32(format!(
            "RegCreateKeyEx(Device Parameters) failed: {}",
            rc.0
        )));
    }

    // SAFETY: `hkey` is open for KEY_SET_VALUE; value bytes outlive the call.
    let rc = unsafe {
        RegSetValueExW(
            hkey,
            PCWSTR(name_w.as_ptr()),
            None,
            REG_DWORD,
            Some(&value.to_le_bytes()),
        )
    };
    // SAFETY: `hkey` was opened above; closed once here.
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    if rc.is_err() {
        return Err(DeviceError::Win32(format!(
            "RegSetValueEx(ControllerIndex) failed: {}",
            rc.0
        )));
    }
    Ok(())
}

/// `UpdateDriverForPlugAndPlayDevicesW(hardware_id, inf)` — bind our driver to
/// the freshly-created node.
fn bind_driver(hardware_id: &str, inf_path: &std::path::Path) -> Result<(), DeviceError> {
    let hw_w = HSTRING::from(hardware_id);
    let inf_w = HSTRING::from(inf_path.to_string_lossy().as_ref());
    let mut reboot = false.into();
    // SAFETY: both strings are NUL-terminated; `reboot` is a valid out-param.
    unsafe {
        UpdateDriverForPlugAndPlayDevicesW(
            None,
            PCWSTR(hw_w.as_ptr()),
            PCWSTR(inf_w.as_ptr()),
            UPDATEDRIVERFORPLUGANDPLAYDEVICES_FLAGS(0),
            Some(&mut reboot),
        )
    }
    .map_err(|e| {
        DeviceError::Win32(format!(
            "UpdateDriverForPlugAndPlayDevices({hardware_id}): {e}"
        ))
    })
}

/// Build a `REG_MULTI_SZ`: each item NUL-terminated, the whole list NUL-terminated.
fn multi_sz(items: &[&str]) -> Vec<u16> {
    let mut out = Vec::new();
    for item in items {
        out.extend(item.encode_utf16());
        out.push(0);
    }
    out.push(0);
    out
}

/// Reinterpret a `u16` buffer as bytes for the SetupAPI property call.
fn as_bytes(v: &[u16]) -> &[u8] {
    // SAFETY: `v` is `len*2` valid bytes; the slice borrows `v` and never outlives it.
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(vid: u16, pid: u16) -> PadNodeSpec {
        PadNodeSpec {
            vid,
            pid,
            driver_pid: pid,
            uses_upper_filter: false,
            requires_xusb_companion: false,
            description: "Test Pad".into(),
            controller_index: 3,
        }
    }

    #[test]
    fn plain_hid_ids() {
        let s = spec(0x054C, 0x0CE6);
        assert_eq!(s.kind(), Kind::PlainHid);
        assert_eq!(s.hardware_id(), "root\\VID_054C&PID_0CE6");
        assert_eq!(s.instance_id(), "ROOT\\HIDClass\\HM_0003");
    }

    #[test]
    fn xinputhid_uses_driver_pid_and_ig00() {
        let mut s = spec(0x045E, 0x0B12);
        s.driver_pid = 0x02FF;
        s.uses_upper_filter = true;
        assert_eq!(s.kind(), Kind::Xinputhid);
        assert_eq!(s.hardware_id(), "root\\VID_045E&PID_02FF&IG_00");
        assert_eq!(s.instance_id(), "ROOT\\VID_045E&PID_02FF&IG_00\\HM_0003");
    }

    #[test]
    fn xbox_legacy_uses_real_pid_and_ig00() {
        let mut s = spec(0x045E, 0x028E);
        s.requires_xusb_companion = true;
        assert_eq!(s.kind(), Kind::XboxLegacy);
        assert_eq!(s.hardware_id(), "root\\VID_045E&PID_028E&IG_00");
    }

    #[test]
    fn container_id_encodes_index() {
        let g = container_id_for(5);
        assert_eq!(g.data1, 0x4849_4430);
        assert_eq!(g.data4, [0x54, 0x52, 0x4F, 0x00, 0x00, 0x00, 0x00, 5]);
    }

    #[test]
    fn multi_sz_is_double_nul_terminated() {
        assert_eq!(
            multi_sz(&["ab", "c"]),
            vec![0x61, 0x62, 0x00, 0x63, 0x00, 0x00]
        );
    }
}

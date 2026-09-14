// SPDX-License-Identifier: GPL-2.0-or-later

//! Timing HID input reports, to price USB/IP against a direct connection.
//!
//! # What this is for
//!
//! `usbip-win2` is attestation signed, so forwarding the Xbox Wireless Adapter to
//! Windows and letting Windows' own driver own it is a live option — one that
//! would delete ~6,500 lines of vendored MT7612U radio and ~450 of GIP from
//! Phase 8, and lift the X360 ceiling on motion, trigger rumble and battery.
//!
//! The cost is that USB/IP is TCP, where an interrupt transfer becomes a network
//! round trip and a retransmit stalls input — against a project that chose raw
//! UDP precisely to avoid that. Whether it matters on a sub-ms LAN is a
//! measurement, and this is it: **the same device, direct versus forwarded.**
//!
//! A pad reports on a fixed interval, so the forwarded distribution widening is
//! the cost, expressed in the units that matter.
//!
//! # Reading it honestly
//!
//! Exercise the device the same way for both runs. A device that only reports on
//! movement will show whatever cadence your hands produced, not the link's, and
//! comparing a fidgety run against a still one measures nothing. Devices that
//! report continuously at their poll rate are the good case here.

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces,
    SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HIDD_ATTRIBUTES, HIDP_CAPS, HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetHidGuid,
    HidD_GetPreparsedData, HidD_GetProductString, HidP_GetCaps, PHIDP_PREPARSED_DATA,
};
use windows::Win32::Foundation::{GENERIC_READ, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile,
};
use windows::core::{HSTRING, PCWSTR};

use sunburst_core::instr::clock;

/// One HID device as enumerated.
struct Device {
    path: String,
    vid: u16,
    pid: u16,
    product: String,
    input_len: u16,
}

fn open(path: &str) -> Result<HANDLE, String> {
    // SAFETY: `path` is a NUL-terminated wide string for the call's lifetime.
    // Sharing read and write is required: the HID stack already has it open.
    unsafe {
        CreateFileW(
            &HSTRING::from(path),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|e| format!("CreateFileW: {e}"))
}

fn describe(path: &str) -> Option<Device> {
    let handle = open(path).ok()?;

    let mut attributes = HIDD_ATTRIBUTES {
        Size: u32::try_from(size_of::<HIDD_ATTRIBUTES>()).expect("fits"),
        ..Default::default()
    };
    // SAFETY: `handle` is a live HID handle and `attributes` is correctly sized.
    let got = unsafe { HidD_GetAttributes(handle, &mut attributes) };

    let mut name = [0u16; 128];
    // SAFETY: `name` is a valid writable buffer of the declared byte length.
    let named = unsafe {
        HidD_GetProductString(
            handle,
            name.as_mut_ptr().cast(),
            u32::try_from(size_of_val(&name)).expect("fits"),
        )
    }
    ;

    // The input report length decides how big a ReadFile buffer has to be; too
    // small and every read fails rather than returning a short report.
    let mut preparsed = PHIDP_PREPARSED_DATA::default();
    let mut input_len = 0u16;
    // SAFETY: `preparsed` is a valid out pointer; freed below if it is filled.
    if unsafe { HidD_GetPreparsedData(handle, &mut preparsed) } {
        let mut caps = HIDP_CAPS::default();
        // SAFETY: `preparsed` came from HidD_GetPreparsedData and `caps` is a
        // valid out parameter.
        if unsafe { HidP_GetCaps(preparsed, &mut caps) }.is_ok() {
            input_len = caps.InputReportByteLength;
        }
        // SAFETY: freed exactly once, and not used afterwards.
        unsafe { HidD_FreePreparsedData(preparsed) };
    }

    // SAFETY: `handle` is ours and closed exactly once.
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(handle);
    }

    if !got {
        return None;
    }
    let len = name.iter().position(|c| *c == 0).unwrap_or(0);
    Some(Device {
        path: path.to_string(),
        vid: attributes.VendorID,
        pid: attributes.ProductID,
        product: if named {
            String::from_utf16_lossy(&name[..len])
        } else {
            String::new()
        },
        input_len,
    })
}

fn enumerate() -> Vec<Device> {
    let mut devices = Vec::new();
    // SAFETY: returns the HID interface class GUID by value.
    let guid = unsafe { HidD_GetHidGuid() };

    // SAFETY: standard SetupAPI enumeration; the set is destroyed below.
    let set = match unsafe {
        SetupDiGetClassDevsW(
            Some(&guid),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    } {
        Ok(set) => set,
        Err(_) => return devices,
    };

    for index in 0.. {
        let mut interface = SP_DEVICE_INTERFACE_DATA {
            cbSize: u32::try_from(size_of::<SP_DEVICE_INTERFACE_DATA>()).expect("fits"),
            ..Default::default()
        };
        // SAFETY: `set` is live and `interface` is correctly sized.
        if unsafe { SetupDiEnumDeviceInterfaces(set, None, &guid, index, &mut interface) }.is_err()
        {
            break;
        }

        // Ask for the size, then the data: the detail struct is variable-length
        // because the device path is inlined at its tail.
        let mut needed = 0u32;
        // SAFETY: a deliberate size query -- passing no buffer is how the API is
        // asked how much it wants.
        let _ = unsafe {
            SetupDiGetDeviceInterfaceDetailW(set, &interface, None, 0, Some(&mut needed), None)
        };
        if needed == 0 {
            continue;
        }
        let mut buffer = vec![0u8; needed as usize];
        let detail = buffer.as_mut_ptr().cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
        // SAFETY: `buffer` is at least `needed` bytes and correctly aligned for
        // the struct, whose cbSize must be the struct header size, not `needed`.
        unsafe {
            (*detail).cbSize =
                u32::try_from(size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>()).expect("fits");
        }
        // SAFETY: as above; the call fills the path at the struct's tail.
        if unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                set,
                &interface,
                Some(detail),
                needed,
                None,
                None,
            )
        }
        .is_err()
        {
            continue;
        }
        // SAFETY: DevicePath is a NUL-terminated wide string inside `buffer`.
        let path = unsafe { PCWSTR((*detail).DevicePath.as_ptr()).to_string() };
        if let Ok(path) = path
            && let Some(device) = describe(&path)
        {
            devices.push(device);
        }
    }

    // SAFETY: `set` is live and destroyed exactly once.
    unsafe {
        let _ = SetupDiDestroyDeviceInfoList(set);
    }
    devices
}

/// Read input reports for `secs`, timing the gap between them.
fn measure(device: &Device, secs: u64) -> Vec<u64> {
    let mut gaps = Vec::new();
    let Ok(handle) = open(&device.path) else {
        println!("  could not open for reading");
        return gaps;
    };

    let len = device.input_len.max(1) as usize;
    let mut buffer = vec![0u8; len];
    let mut previous: Option<u64> = None;
    let deadline = clock::now() + clock::ticks_per_sec() * secs;

    while clock::now() < deadline {
        let mut read = 0u32;
        // SAFETY: `buffer` is at least the input report length, which is what
        // ReadFile on a HID device requires, and `read` is a valid out pointer.
        let ok = unsafe { ReadFile(handle, Some(buffer.as_mut_slice()), Some(&mut read), None) };
        if ok.is_err() {
            break;
        }
        let now = clock::now();
        if let Some(previous) = previous {
            gaps.push(clock::ticks_to_ns(now - previous));
        }
        previous = Some(now);
    }

    // SAFETY: ours, closed exactly once.
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(handle);
    }
    gaps.sort_unstable();
    gaps
}

fn percentile(sorted: &[u64], pct: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[(sorted.len() * pct / 100).min(sorted.len() - 1)]
}

pub fn run() {
    println!("== HID input report intervals ==");
    println!("  For pricing USB/IP: run this with the device connected directly, then");
    println!("  again with it forwarded, and compare. Exercise it the same way both");
    println!("  times -- a device that only reports on movement shows your hands, not");
    println!("  the link.");
    println!();

    let devices = enumerate();
    if devices.is_empty() {
        println!("  no HID devices enumerated");
        return;
    }

    // Select by list index, not VID:PID. A single physical device commonly
    // exposes several HID collections -- a keyboard here shows five under one
    // VID:PID -- so selecting by identifier silently picks whichever came first,
    // which may not be the one that carries input.
    let wanted: Option<usize> = std::env::args()
        .skip_while(|a| a != "--hidreport")
        .nth(1)
        .and_then(|a| a.parse().ok());

    let Some(chosen) = wanted.and_then(|i| i.checked_sub(1)).filter(|i| *i < devices.len())
    else {
        println!("  {} HID device(s). Pick one by number:", devices.len());
        println!("    probe-windows --hidreport <n>");
        println!();
        for (index, device) in devices.iter().enumerate() {
            let note = if device.input_len == 0 {
                "  (no input report -- cannot be timed)"
            } else {
                ""
            };
            println!(
                "    {:>2}  {:04x}:{:04x}  input {:>4}B  {}{}",
                index + 1,
                device.vid,
                device.pid,
                device.input_len,
                device.product,
                note
            );
        }
        println!();
        println!("  Several entries sharing one VID:PID are separate collections of the same");
        println!("  physical device, which is why this selects by number.");
        println!();
        println!("  Note the Xbox Wireless Adapter (045e:02e6) will NOT appear here: it is");
        println!("  not a HID device but an MT7612U radio, so forward the adapter and time");
        println!("  the pads that show up through it instead.");
        return;
    };
    let device = &devices[chosen];

    println!(
        "  {:04x}:{:04x} {} -- input report {}B",
        device.vid, device.pid, device.product, device.input_len
    );
    if device.input_len == 0 {
        println!("  -> No input report length; ReadFile cannot be sized and this device");
        println!("     cannot be timed this way.");
        return;
    }

    println!("  reading for 10s -- use the device");
    let gaps = measure(device, 10);
    if gaps.is_empty() {
        println!("  no reports arrived. A device that reports only on input needs using.");
        return;
    }
    println!(
        "  {} intervals: p50 {:.2}ms  p99 {:.2}ms  max {:.2}ms",
        gaps.len(),
        percentile(&gaps, 50) as f64 / 1e6,
        percentile(&gaps, 99) as f64 / 1e6,
        gaps[gaps.len() - 1] as f64 / 1e6,
    );
    if gaps.len() < 100 {
        println!("  (under 100 samples -- p99 is the max here, not a percentile)");
    }
    println!();
    println!("  -> p50 is the device's poll interval; p99 and max are what the link adds.");
    println!("     A forwarded device should show the same p50 and a worse tail. If p50");
    println!("     itself moves, the transport is rate-limiting rather than jittering.");
}

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
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile,
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

fn open_overlapped(path: &str) -> Result<HANDLE, String> {
    // SAFETY: `path` is a NUL-terminated wide string for the call's lifetime.
    // FILE_FLAG_OVERLAPPED is what makes a read cancellable: a synchronous
    // ReadFile on a HID device blocks until a report arrives, so a device that
    // never reports parks the caller forever and no deadline can fire.
    unsafe {
        CreateFileW(
            &HSTRING::from(path),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            None,
        )
    }
    .map_err(|e| format!("CreateFileW: {e}"))
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
///
/// Overlapped, with a bounded wait per read. The first version used a plain
/// synchronous `ReadFile` inside a loop that checked a deadline between
/// iterations — so a device that reported nothing blocked on the very first call
/// and the timeout could never be reached. It froze rather than reporting that
/// nothing arrived, which is the one outcome the caller most needs told.
fn measure(device: &Device, secs: u64) -> Vec<u64> {
    use windows::Win32::Foundation::{GetLastError, ERROR_IO_PENDING, WAIT_OBJECT_0};
    use windows::Win32::System::IO::{CancelIo, GetOverlappedResult, OVERLAPPED};
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

    let mut gaps = Vec::new();
    let Ok(handle) = open_overlapped(&device.path) else {
        println!("  could not open for reading");
        return gaps;
    };

    // Manual-reset event, reset explicitly before each read.
    // SAFETY: no security attributes, no name; a valid event handle or an error.
    let Ok(event) = (unsafe { CreateEventW(None, true, false, None) }) else {
        println!("  could not create the completion event");
        // SAFETY: ours, closed once.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(handle);
        }
        return gaps;
    };

    let len = device.input_len.max(1) as usize;
    let mut buffer = vec![0u8; len];
    let mut previous: Option<u64> = None;
    let deadline = clock::now() + clock::ticks_per_sec() * secs;

    while clock::now() < deadline {
        // SAFETY: `event` is a live manual-reset event.
        unsafe {
            let _ = windows::Win32::System::Threading::ResetEvent(event);
        }
        let mut overlapped = OVERLAPPED {
            hEvent: event,
            ..Default::default()
        };

        // SAFETY: `buffer` is at least the input report length, `overlapped`
        // outlives the operation, and the handle was opened overlapped.
        let started = unsafe {
            ReadFile(handle, Some(buffer.as_mut_slice()), None, Some(&mut overlapped))
        };
        if started.is_err() {
            // SAFETY: reading a thread-local error code.
            if unsafe { GetLastError() } != ERROR_IO_PENDING {
                break;
            }
        }

        // Wait only as long as is left, so the total honours `secs` even when
        // the device is silent.
        let remaining_ns = clock::ticks_to_ns(deadline.saturating_sub(clock::now()));
        let wait_ms = u32::try_from(remaining_ns / 1_000_000).unwrap_or(u32::MAX).max(1);
        // SAFETY: `event` is live; the wait is bounded.
        let waited = unsafe { WaitForSingleObject(event, wait_ms) };
        if waited != WAIT_OBJECT_0 {
            // SAFETY: cancels the pending read on this handle before the
            // OVERLAPPED goes out of scope, which it must.
            unsafe {
                let _ = CancelIo(handle);
            }
            break;
        }

        let mut read = 0u32;
        // SAFETY: the operation completed; `read` is a valid out pointer.
        if unsafe { GetOverlappedResult(handle, &overlapped, &mut read, false) }.is_err() {
            break;
        }

        let now = clock::now();
        if let Some(previous) = previous {
            gaps.push(clock::ticks_to_ns(now - previous));
        }
        previous = Some(now);
    }

    // SAFETY: both handles are ours and closed exactly once.
    unsafe {
        let _ = CancelIo(handle);
        let _ = windows::Win32::Foundation::CloseHandle(event);
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

    // Accept either a number or a vid:pid. Neither alone is enough: a single
    // physical device commonly exposes several HID collections under one
    // identifier -- a keyboard here shows five -- while enumeration order is not
    // guaranteed stable between runs, so a number is only meaningful against the
    // listing that produced it.
    let arg = std::env::args()
        .skip_while(|a| a != "--hidreport")
        .nth(1);

    let timeable = |d: &&Device| d.input_len > 0;
    let list = |devices: &[Device]| {
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
    };

    let chosen = match arg.as_deref() {
        // vid:pid -- unambiguous only when one collection under it can be timed.
        Some(a) if a.contains(':') => {
            let parsed = a.split_once(':').and_then(|(v, p)| {
                Some((
                    u16::from_str_radix(v.trim_start_matches("0x"), 16).ok()?,
                    u16::from_str_radix(p.trim_start_matches("0x"), 16).ok()?,
                ))
            });
            let Some((vid, pid)) = parsed else {
                println!("  could not parse '{a}' as vid:pid (hex, e.g. 045e:02ff)");
                return;
            };
            let matches: Vec<usize> = devices
                .iter()
                .enumerate()
                .filter(|(_, d)| d.vid == vid && d.pid == pid && timeable(d))
                .map(|(i, _)| i)
                .collect();
            match matches.as_slice() {
                [] => {
                    println!("  {vid:04x}:{pid:04x} has no timeable collection. Present:");
                    list(&devices);
                    return;
                }
                [only] => *only,
                several => {
                    println!(
                        "  {vid:04x}:{pid:04x} has {} timeable collections -- pick by number:",
                        several.len()
                    );
                    list(&devices);
                    return;
                }
            }
        }
        Some(a) => match a.parse::<usize>().ok().and_then(|n| n.checked_sub(1)) {
            Some(index) if index < devices.len() => index,
            _ => {
                println!("  '{a}' is not a device number or a vid:pid.");
                list(&devices);
                return;
            }
        },
        None => {
            println!("  {} HID device(s). Pick one:", devices.len());
            println!("    probe-windows --hidreport <n>         by number, from this listing");
            println!("    probe-windows --hidreport 045e:02ff   by id, when it is unambiguous");
            println!();
            list(&devices);
            println!();
            println!("  Several entries sharing one VID:PID are separate collections of the");
            println!("  same physical device. Enumeration order is not guaranteed stable");
            println!("  between runs, so re-list rather than reusing an old number.");
            println!();
            println!("  The Xbox Wireless Adapter (045e:02e6) will NOT appear here: it is not");
            println!("  a HID device but an MT7612U radio, so forward the adapter and time the");
            println!("  pads that show up through it instead.");
            return;
        }
    };
    let device = &devices[chosen];

    println!(
        "  {:04x}:{:04x} {} -- input report {}B",
        device.vid, device.pid, device.product, device.input_len
    );
    if device.input_len == 0 {
        println!("  -> No input report length, so ReadFile cannot be sized and this device");
        println!("     cannot be timed. These can be:");
        for (index, other) in devices.iter().enumerate().filter(|(_, d)| d.input_len > 0) {
            println!(
                "    {:>2}  {:04x}:{:04x}  input {:>4}B  {}",
                index + 1,
                other.vid,
                other.pid,
                other.input_len,
                other.product
            );
        }
        return;
    }

    println!("  reading for 10s -- use the device");
    let gaps = measure(device, 10);
    if gaps.is_empty() {
        println!("  no reports arrived in {}s.", 10);
        println!();
        println!("  -> Two likely reasons. A device that reports only on input needs using");
        println!("     while this runs. Or this is a *virtual* pad, in which case it emits");
        println!("     nothing unless something is feeding it -- close PadForge and re-list:");
        println!("     if 045e:02ff disappears it was virtual, and its intervals would have");
        println!("     been PadForge's submit cadence rather than a controller's anyway.");
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

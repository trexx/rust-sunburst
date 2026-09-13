// SPDX-License-Identifier: GPL-2.0-or-later

//! Can HIDMaestro be driven from Rust?
//!
//! # Why this is a spike and not an integration
//!
//! ViGEmBus was retired and archived in November 2023. HIDMaestro is the live
//! alternative and reaches what ViGEm structurally cannot — DirectInput, XInput,
//! SDL3 *and* WGI/GameInput, with byte-exact VID/PID per profile, which is the
//! ceiling currently forcing motion, trigger rumble and battery out of Phase 8's
//! scope. It is also five months old, and using it has one hard condition.
//!
//! **Creating the device is not reimplementable in Rust.** Its `Internal/` is
//! ~600KB of C#; `DeviceOrchestrator.cs` alone is 147KB, and its `INTERNALS.md`
//! describes `SwDeviceCreate` with non-sentinel ContainerIDs, an
//! `UpperFilters = "xinputhid"` registry tripwire found by Ghidra-decompiling
//! `Windows.Gaming.Input.dll`, a slot-1-skip workaround from decompiling
//! `xinput1_4.dll`, `BTHLEDEVICE` spoofing and per-profile driver generation.
//! None of that is worth re-deriving.
//!
//! So the only viable shape is **their C# owning device lifecycle, Rust writing
//! reports**, and the question this module answers is whether that second half
//! works at all. If Rust cannot write the section, HIDMaestro is only reachable
//! by writing our pad path in C#, and the answer is no.
//!
//! # The layout
//!
//! Transcribed from `sdk/HIDMaestro.Core/Internal/SharedMemoryIO.cs`. Both
//! directions are shared memory, which is the encouraging part: no .NET on the
//! hot path, and rumble does not need their `OutputDecoded` event.

use std::ffi::c_void;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Memory::{
    FILE_MAP_READ, FILE_MAP_WRITE, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile,
};
use windows::core::HSTRING;

// ---------------------------------------------------------------- input layout

/// `SeqNo` — a seqlock. Odd while a write is in flight, even when settled, the
/// same discipline the latency harness uses for the presenter's flip pair.
const IN_SEQNO: usize = 0;
const IN_DATA_SIZE: usize = 4;
const IN_DATA: usize = 8;
const IN_DATA_CAPACITY: usize = 256;
const IN_GIP_DATA: usize = 264;
const IN_GIP_LENGTH: usize = 14;
const IN_EXT_SIZE: usize = 278;
const IN_EXT_DATA: usize = 282;
const IN_EXT_CAPACITY: usize = 80;
/// Total input section size.
const INPUT_SIZE: usize = IN_EXT_DATA + IN_EXT_CAPACITY;

// Every offset above checks the one before it, so the whole layout has to agree
// with itself or the build stops. Transcribed offsets that merely sit in a
// comment are the ones that drift; these cannot.
const _: () = assert!(IN_DATA == IN_DATA_SIZE + 4);
const _: () = assert!(IN_GIP_DATA == IN_DATA + IN_DATA_CAPACITY);
const _: () = assert!(IN_EXT_SIZE == IN_GIP_DATA + IN_GIP_LENGTH);
const _: () = assert!(IN_EXT_DATA == IN_EXT_SIZE + 4);
const _: () = assert!(INPUT_SIZE == 362);
const _: () = assert!(IN_SEQNO == 0);

// --------------------------------------------------------------- output layout

/// Where rumble arrives: a 64-slot ring, so a burst of output reports cannot
/// overwrite the one being read.
const OUT_HEAD: usize = 0;
const OUT_HEADER_SIZE: usize = 8;
const OUT_RING_SLOTS: usize = 64;
const OUT_SLOT_SIZE: usize = 4 + 1 + 1 + 2 + 256;
const OUT_SLOT_SEQNO: usize = 0;
const OUT_SLOT_SOURCE: usize = 4;
const OUT_SLOT_REPORT_ID: usize = 5;
const OUT_SLOT_SIZE_OFF: usize = 6;
const OUT_SLOT_DATA: usize = 8;
const OUTPUT_SIZE: usize = OUT_HEADER_SIZE + OUT_RING_SLOTS * OUT_SLOT_SIZE;

const _: () = assert!(OUT_HEAD == 0);
const _: () = assert!(OUT_SLOT_SOURCE == OUT_SLOT_SEQNO + 4);
const _: () = assert!(OUT_SLOT_REPORT_ID == OUT_SLOT_SOURCE + 1);
const _: () = assert!(OUT_SLOT_SIZE_OFF == OUT_SLOT_REPORT_ID + 1);
const _: () = assert!(OUT_SLOT_DATA == OUT_SLOT_SIZE_OFF + 2);
const _: () = assert!(OUT_SLOT_SIZE == OUT_SLOT_DATA + 256);
const _: () = assert!(OUTPUT_SIZE == 16904);

/// One mapped section.
struct Section {
    handle: HANDLE,
    base: *mut u8,
    len: usize,
}

impl Section {
    /// Open an existing section by name. Deliberately *not* created here: the
    /// section belongs to whoever created the pad, and creating one ourselves
    /// would prove nothing except that we can call `CreateFileMapping`.
    fn open(name: &str, write: bool, len: usize) -> Result<Section, String> {
        let access = if write {
            FILE_MAP_READ | FILE_MAP_WRITE
        } else {
            FILE_MAP_READ
        };
        // SAFETY: `name` is a NUL-terminated wide string for the call's lifetime.
        let handle = unsafe { OpenFileMappingW(access.0, false, &HSTRING::from(name)) }
            .map_err(|e| format!("OpenFileMappingW({name}): {e}"))?;

        // SAFETY: `handle` is a live section handle and `len` is within it.
        let view = unsafe { MapViewOfFile(handle, access, 0, 0, len) };
        if view.Value.is_null() {
            return Err(format!("MapViewOfFile({name}) returned null"));
        }
        Ok(Section {
            handle,
            base: view.Value.cast(),
            len,
        })
    }

    fn read_u32(&self, offset: usize) -> u32 {
        debug_assert!(offset + 4 <= self.len);
        // SAFETY: `offset + 4` is within the mapped view, and the section is
        // written by another process so every read is volatile.
        unsafe { std::ptr::read_volatile(self.base.add(offset).cast::<u32>()) }
    }

    fn write_u32(&self, offset: usize, value: u32) {
        debug_assert!(offset + 4 <= self.len);
        // SAFETY: as above, and the section was mapped writable.
        unsafe { std::ptr::write_volatile(self.base.add(offset).cast::<u32>(), value) }
    }

    fn write_bytes(&self, offset: usize, bytes: &[u8]) {
        debug_assert!(offset + bytes.len() <= self.len);
        // SAFETY: the destination range is inside the mapped view.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.base.add(offset), bytes.len()) }
    }

    fn read_bytes(&self, offset: usize, out: &mut [u8]) {
        debug_assert!(offset + out.len() <= self.len);
        // SAFETY: the source range is inside the mapped view.
        unsafe { std::ptr::copy_nonoverlapping(self.base.add(offset), out.as_mut_ptr(), out.len()) }
    }
}

impl Drop for Section {
    fn drop(&mut self) {
        // SAFETY: `base` came from MapViewOfFile and is unmapped exactly once.
        unsafe {
            let _ = UnmapViewOfFile(windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.base.cast::<c_void>(),
            });
            let _ = windows::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

/// Write one input report under the seqlock.
///
/// Odd while writing, even when settled, so the driver's reader can tell a torn
/// frame from a complete one.
fn submit(input: &Section, report: &[u8]) {
    let seq = input.read_u32(IN_SEQNO);
    input.write_u32(IN_SEQNO, seq.wrapping_add(1) | 1);

    let len = report.len().min(IN_DATA_CAPACITY);
    input.write_bytes(IN_DATA, &report[..len]);
    input.write_u32(IN_DATA_SIZE, len as u32);
    // Nothing in this spike uses the GIP or extended regions; leaving them alone
    // rather than zeroing avoids disturbing whatever the owner put there.

    // Settle to an even value one past where we started.
    input.write_u32(IN_SEQNO, (seq | 1).wrapping_add(1));
}

/// Drain whatever output reports the ring holds beyond `seen`, returning the new
/// head.
fn drain_output(output: &Section, seen: u32, found: &mut Vec<(u8, u8, Vec<u8>)>) -> u32 {
    let head = output.read_u32(OUT_HEAD);
    if head == seen {
        return head;
    }
    // Only the most recent OUT_RING_SLOTS are still present; anything older has
    // been overwritten and is not worth reporting as lost.
    let first = head.saturating_sub(OUT_RING_SLOTS as u32).max(seen);
    for seq in first..head {
        let slot = OUT_HEADER_SIZE + (seq as usize % OUT_RING_SLOTS) * OUT_SLOT_SIZE;
        // Re-check the slot's own sequence: a slot recycled mid-read is stale.
        if output.read_u32(slot + OUT_SLOT_SEQNO) != seq {
            continue;
        }
        let mut meta = [0u8; 4];
        output.read_bytes(slot + OUT_SLOT_SOURCE, &mut meta[..1]);
        let source = meta[0];
        output.read_bytes(slot + OUT_SLOT_REPORT_ID, &mut meta[..1]);
        let report_id = meta[0];
        let size = u32::from(output.read_u32(slot + OUT_SLOT_SIZE_OFF) as u16) as usize;
        let size = size.min(256);
        let mut data = vec![0u8; size];
        output.read_bytes(slot + OUT_SLOT_DATA, &mut data);
        found.push((source, report_id, data));
    }
    head
}

/// A recognisable pattern, so `joy.cpl` shows unambiguously whether the frames
/// landed rather than leaving it to interpretation.
///
/// Sticks to known extremes and one button at a time: a wrong byte order shows
/// up as the wrong axis moving, which is far easier to read than a subtly
/// wrong value.
fn pattern(step: u32) -> [u8; 14] {
    let mut report = [0u8; 14];
    // Byte 0 is conventionally the report ID for these profiles; leave it and
    // let the owner's descriptor decide, since guessing it wrong is the most
    // likely reason nothing moves.
    let phase = step % 4;
    let (lx, ly): (i16, i16) = match phase {
        0 => (i16::MAX, 0),
        1 => (0, i16::MAX),
        2 => (i16::MIN, 0),
        _ => (0, i16::MIN),
    };
    report[1..3].copy_from_slice(&lx.to_le_bytes());
    report[3..5].copy_from_slice(&ly.to_le_bytes());
    // One button per phase, so a shifted button word is visible as the wrong
    // button rather than as nothing.
    let button = 1u16 << phase;
    report[5..7].copy_from_slice(&button.to_le_bytes());
    report
}

pub fn run() {
    println!("== HIDMaestro: can Rust drive it? ==");
    println!("  Creating a pad needs their C# orchestrator -- ~600KB including");
    println!("  Ghidra-derived Windows PnP workarounds -- so this only tests the half");
    println!("  that would be ours: writing reports and reading rumble back.");
    println!();
    println!("  Create a pad with HIDMaestro's own tooling first, and leave joy.cpl open.");
    println!();

    // Index 0 only: if the first pad's sections are not there, a second will not
    // be either, and reporting four identical failures is noise.
    let input = Section::open("Global\\HIDMaestroInput0", true, INPUT_SIZE);
    let output = Section::open("Global\\HIDMaestroOutput0", false, OUTPUT_SIZE);

    let input = match input {
        Ok(section) => {
            println!("  input  section: open, SeqNo {}", section.read_u32(IN_SEQNO));
            section
        }
        Err(e) => {
            println!("  input  section: {e}");
            println!();
            println!("  -> No pad exists, or it is owned by another session. `Global\\` sections");
            println!("     are per-session; a pad created by an elevated process may not be");
            println!("     visible here. This is a different failure from one that opens and");
            println!("     ignores writes, which is why it is reported separately.");
            return;
        }
    };

    let output = match output {
        Ok(section) => {
            println!("  output section: open, Head {}", section.read_u32(OUT_HEAD));
            Some(section)
        }
        Err(e) => {
            println!("  output section: {e}");
            println!("  -> Rumble would then need their C# OutputDecoded event, which makes");
            println!("     the sidecar bigger than 'create pads at startup'.");
            None
        }
    };

    println!();
    println!("  Writing 40 frames over 4 seconds: stick to each extreme in turn, one");
    println!("  button per phase. Watch joy.cpl.");

    let mut seen = output.as_ref().map_or(0, |s| s.read_u32(OUT_HEAD));
    let mut rumble = Vec::new();
    let start_seq = input.read_u32(IN_SEQNO);

    for step in 0..40u32 {
        submit(&input, &pattern(step));
        if let Some(output) = &output {
            seen = drain_output(output, seen, &mut rumble);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let end_seq = input.read_u32(IN_SEQNO);
    println!();
    println!("  SeqNo moved {start_seq} -> {end_seq}");
    if end_seq == start_seq {
        println!("  -> Our own writes did not stick, which means the section is not writable");
        println!("     by this process. Check whether the pad's owner is elevated.");
    }

    if let Some(output) = &output {
        println!("  output Head now {}", output.read_u32(OUT_HEAD));
        if rumble.is_empty() {
            println!("  no output reports seen -- trigger vibration in a game while this runs");
            println!("  to find out whether rumble is reachable without their C# event.");
        } else {
            println!("  {} output report(s) seen:", rumble.len());
            for (source, report_id, data) in rumble.iter().take(8) {
                let head: Vec<String> =
                    data.iter().take(8).map(|b| format!("{b:02x}")).collect();
                println!(
                    "    source {source} report {report_id} len {} [{}]",
                    data.len(),
                    head.join(" ")
                );
            }
            println!("  -> Rumble IS reachable from shared memory. No .NET on the hot path.");
        }
    }

    println!();
    println!("  Then: does the pad survive HIDMaestro's tooling exiting? That decides");
    println!("  whether this is 'run a helper at startup' or 'supervise a .NET service'.");
}

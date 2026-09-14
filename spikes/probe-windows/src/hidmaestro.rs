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

use sunburst_core::instr::clock;

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

/// Is this process elevated?
///
/// It matters more than it looks. PadForge — the app that creates these pads —
/// **always runs elevated**, and installs HIDMaestro inside that elevated
/// session. An object created at high integrity is not writable from a medium
/// one, so a non-elevated probe can fail here for a reason that has nothing to
/// do with whether the approach works.
fn elevated() -> Option<bool> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = HANDLE::default();
    // SAFETY: GetCurrentProcess returns a pseudo-handle needing no close, and
    // `token` is a valid out parameter.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.ok()?;

    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0u32;
    // SAFETY: `elevation` is correctly sized for TokenElevation and `returned`
    // is a valid out pointer.
    let got = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some((&raw mut elevation).cast()),
            u32::try_from(size_of::<TOKEN_ELEVATION>()).expect("fits"),
            &mut returned,
        )
    };
    // SAFETY: ours, closed exactly once.
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(token);
    }
    got.ok()?;
    Some(elevation.TokenIsElevated != 0)
}

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

    /// `DataSize` sits at slot+6, which is not 4-aligned, so this goes through a
    /// byte copy rather than a wider volatile read — the latter is undefined
    /// behaviour on an unaligned address even where x86 would tolerate it.
    fn read_u16(&self, offset: usize) -> u16 {
        let mut bytes = [0u8; 2];
        self.read_bytes(offset, &mut bytes);
        u16::from_le_bytes(bytes)
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
fn submit(input: &Section, report: &[u8]) -> bool {
    let seq = input.read_u32(IN_SEQNO);
    input.write_u32(IN_SEQNO, seq.wrapping_add(1) | 1);

    let len = report.len().min(IN_DATA_CAPACITY);
    input.write_bytes(IN_DATA, &report[..len]);
    input.write_u32(IN_DATA_SIZE, len as u32);
    // Nothing in this spike uses the GIP or extended regions; leaving them alone
    // rather than zeroing avoids disturbing whatever the owner put there.

    // Settle to an even value one past where we started.
    input.write_u32(IN_SEQNO, (seq | 1).wrapping_add(1));

    // Read straight back. This is the only way to answer "can Rust drive it"
    // without depending on what joy.cpl looks like: if the bytes are still ours
    // the write reached the section, and if they are not, we are being raced —
    // which is a different finding, not a failure to write.
    let mut echo = vec![0u8; len];
    input.read_bytes(IN_DATA, &mut echo);
    echo == report[..len]
}

/// Drain whatever output reports the ring holds beyond `seen`, returning the new
/// head.
fn drain_output(
    output: &Section,
    seen: u32,
    found: &mut Vec<(u8, u8, Vec<u8>)>,
    mismatched: &mut Vec<(u32, u32)>,
) -> u32 {
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
        let slot_seq = output.read_u32(slot + OUT_SLOT_SEQNO);
        if slot_seq != seq {
            // Report rather than skip. Head advanced 37 -> 41 on the first real
            // run while this loop found nothing, which means the assumption that
            // a slot's SeqNo equals its ring position is wrong — and silently
            // `continue`ing hid exactly the evidence needed to fix it.
            mismatched.push((seq, slot_seq));
            continue;
        }
        let mut meta = [0u8; 4];
        output.read_bytes(slot + OUT_SLOT_SOURCE, &mut meta[..1]);
        let source = meta[0];
        output.read_bytes(slot + OUT_SLOT_REPORT_ID, &mut meta[..1]);
        let report_id = meta[0];
        let size = usize::from(output.read_u16(slot + OUT_SLOT_SIZE_OFF)).min(256);
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

/// Every name the SDK builds, from `SharedMemoryIO.cs`. Indices are per
/// controller, and nothing guarantees the first pad is index 0.
const NAME_PATTERNS: [(&str, bool); 6] = [
    ("Global\\HIDMaestroInput", true),
    ("Global\\HIDMaestroOutput", true),
    ("Global\\HIDMaestroPidState", true),
    ("Global\\HIDMaestroInputEvent", false),
    ("Global\\HIDMaestroOutputEvent", false),
    ("Global\\HIDMaestroCompanionInputEvent", false),
];

/// How many controller indices to look at. The SDK numbers per controller and
/// a remapper may not start at zero.
const SURVEY_INDICES: u32 = 16;

/// How long to wait for a section to appear once a pad is known to exist.
///
/// **The sections are created lazily.** `SharedMemoryIO.EnsureInputMapping`
/// builds the input section and both its events together, on first call — and
/// that call happens when something first submits state for the controller. A
/// pad that exists but has never been fed has no section at all, which is what
/// the first run of this found: `HIDMaestroCompanionInputEvent0` alone, because
/// the XUSB companion's driver open-or-creates that name when its devnode comes
/// up, independently of the SDK.
const WAIT_SECS: u64 = 30;

/// Report which of HIDMaestro's named objects actually exist.
///
/// Hard-coding index 0 produced `ERROR_FILE_NOT_FOUND` and no information: the
/// name was right — `SharedMemoryIO.cs` builds exactly this — so "not found"
/// meant either a different index or nothing there at all, and one name cannot
/// tell those apart. Surveying can.
///
/// An event present without its section is especially diagnostic: it means a pad
/// exists and something has not mapped its memory yet.
fn survey() -> Vec<String> {
    use windows::Win32::System::Threading::{OpenEventW, SYNCHRONIZATION_SYNCHRONIZE};

    let mut found = Vec::new();
    for (prefix, is_section) in NAME_PATTERNS {
        for index in 0..SURVEY_INDICES {
            let name = format!("{prefix}{index}");
            let exists = if is_section {
                // SAFETY: `name` is a NUL-terminated wide string for the call.
                unsafe { OpenFileMappingW(FILE_MAP_READ.0, false, &HSTRING::from(&name)) }
                    .map(|h| {
                        // SAFETY: ours, closed once; only existence was wanted.
                        unsafe {
                            let _ = windows::Win32::Foundation::CloseHandle(h);
                        }
                    })
                    .is_ok()
            } else {
                // SAFETY: as above; SYNCHRONIZE is the least access that proves
                // the object is there.
                unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, &HSTRING::from(&name)) }
                    .map(|h| {
                        // SAFETY: ours, closed once.
                        unsafe {
                            let _ = windows::Win32::Foundation::CloseHandle(h);
                        }
                    })
                    .is_ok()
            };
            if exists {
                found.push(name);
            }
        }
    }
    found
}

pub fn run() {
    println!("== HIDMaestro: can Rust drive it? ==");
    println!("  Creating a pad needs their C# orchestrator -- ~600KB including");
    println!("  Ghidra-derived Windows PnP workarounds -- so this only tests the half");
    println!("  that would be ours: writing reports and reading rumble back.");
    println!();
    println!("  Create a pad first -- PadForge is the app that does it -- and leave");
    println!("  joy.cpl open.");
    match elevated() {
        Some(true) => println!("  this process: elevated"),
        Some(false) => {
            println!("  this process: NOT elevated");
            println!("  PadForge runs elevated and creates the pad there, so if the sections");
            println!("  below fail to open or writes do not stick, re-run this as Administrator");
            println!("  before concluding anything about the approach.");
        }
        None => println!("  this process: elevation unknown"),
    }
    println!();

    // Survey before asserting. Index 0 is a guess, and a failed guess at one
    // name carries no information about why.
    let found = survey();
    if found.is_empty() {
        println!("  no HIDMaestro named objects found at indices 0..{SURVEY_INDICES}.");
        println!();
        println!("  -> Nothing is there to talk to. The name is not the problem: the SDK's");
        println!("     SharedMemoryIO.cs builds exactly these names. So either no pad exists");
        println!("     right now, or the app that made it has exited.");
        println!();
        println!("     PadForge creates these sections itself -- the driver cannot, it lacks");
        println!("     SeCreateGlobalPrivilege -- so **PadForge has to still be running**.");
        println!("     If it was closed after creating the pad, that is itself the answer to");
        println!("     the lifetime question: a sidecar must stay resident.");
        return;
    }
    println!("  found {} HIDMaestro object(s):", found.len());
    for name in &found {
        println!("    {name}");
    }

    // Drive the lowest index that has an input section, rather than assuming 0.
    let input_index = |names: &[String]| {
        (0..SURVEY_INDICES)
            .find(|i| names.iter().any(|n| n == &format!("Global\\HIDMaestroInput{i}")))
    };

    let index = match input_index(&found) {
        Some(index) => index,
        None => {
            // Not a dead end: the section is built on first submit, so a pad
            // that exists but has never been fed simply has not got one yet.
            println!();
            println!("  No input section yet -- but a companion event without one is exactly");
            println!("  what a pad that has never been fed looks like. The SDK builds the");
            println!("  section on its first submit, so:");
            println!();
            println!("  >>> In PadForge, bind a physical controller to this virtual pad and");
            println!("  >>> move a stick. Waiting up to {WAIT_SECS}s for the section to appear.");
            println!();

            let deadline = clock::now() + clock::ticks_per_sec() * WAIT_SECS;
            let mut appeared = None;
            while clock::now() < deadline {
                let names = survey();
                if let Some(index) = input_index(&names) {
                    appeared = Some(index);
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            match appeared {
                Some(index) => {
                    println!("  section appeared at index {index}");
                    index
                }
                None => {
                    println!("  Still nothing.");
                    println!();
                    println!("  -> Either no input reached the pad, or PadForge submits through a");
                    println!("     path that does not use these sections at all. Both are real");
                    println!("     answers; neither is 'the approach cannot work', which is what");
                    println!("     this probe wrongly implied last run.");
                    return;
                }
            }
        }
    };
    println!();
    println!("  driving controller index {index}");

    let input = Section::open(&format!("Global\\HIDMaestroInput{index}"), true, INPUT_SIZE);
    let output = Section::open(&format!("Global\\HIDMaestroOutput{index}"), false, OUTPUT_SIZE);

    let input = match input {
        Ok(section) => {
            println!("  input  section: open, SeqNo {}", section.read_u32(IN_SEQNO));
            section
        }
        Err(e) => {
            println!("  input  section: {e}");
            println!();
            println!("  -> No pad exists, or this process cannot reach it. `Global\\` sections");
            println!("     are per-session, and an object created at high integrity is not");
            println!("     writable from a medium one -- PadForge creates the pad elevated.");
            println!("     Re-run as Administrator before reading this as a real answer.");
            println!("     Note this is a different failure from a section that opens and");
            println!("     ignores writes, which is why the two are reported separately.");
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
    println!("  Writing 400 frames over 4 seconds. PadForge submits for this pad too,");
    println!("  so ours are outnumbered -- every write is read straight back to see");
    println!("  whether it reached the section at all. Watch joy.cpl as well.");

    let mut seen = output.as_ref().map_or(0, |s| s.read_u32(OUT_HEAD));
    let mut rumble = Vec::new();
    let mut mismatched = Vec::new();
    let start_seq = input.read_u32(IN_SEQNO);

    // 400 at 10ms rather than 40 at 100ms: at ~60Hz of competing submissions a
    // 10Hz pattern is invisible in joy.cpl, and the read-back needs to land
    // before the next writer gets there.
    const FRAMES: u32 = 400;
    let mut survived = 0u32;
    for step in 0..FRAMES {
        if submit(&input, &pattern(step)) {
            survived += 1;
        }
        if let Some(output) = &output {
            seen = drain_output(output, seen, &mut rumble, &mut mismatched);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // One more: reports arriving in the final sleep would otherwise be missed,
    // which is how the first run lost all four of them.
    if let Some(output) = &output {
        drain_output(output, seen, &mut rumble, &mut mismatched);
    }

    let end_seq = input.read_u32(IN_SEQNO);
    let moved = end_seq.wrapping_sub(start_seq);
    let ours = FRAMES * 2;
    println!();
    println!("  SeqNo moved {start_seq} -> {end_seq} (delta {moved})");
    println!("  our {FRAMES} writes account for ~{ours} of that");
    if moved > ours {
        let others = moved - ours;
        println!(
            "  -> ~{others} increments came from elsewhere: PadForge is submitting for this",
        );
        println!("     pad too. Expected, and it is why the read-back below matters more than");
        println!("     anything joy.cpl shows.");
    }

    println!();
    println!("  read-back: {survived}/{FRAMES} of our writes were still ours when read");
    if survived == 0 {
        println!("  -> Nothing we wrote survived. Either the section is not writable by this");
        println!("     process despite opening, or a co-writer overwrites within microseconds.");
        println!("     Unbind the physical controller in PadForge and re-run: if writes then");
        println!("     stick, Rust can drive it and the only issue was the race.");
    } else if survived < FRAMES / 2 {
        println!("  -> Writes land but are frequently overwritten. Rust CAN drive the section;");
        println!("     a real integration would be the only writer, so this is the race and");
        println!("     not a limit.");
    } else {
        println!("  -> Rust can drive the section. This is the question the spike existed for,");
        println!("     and the answer is yes.");
    }

    if let Some(output) = &output {
        println!();
        println!("  output Head now {}", output.read_u32(OUT_HEAD));
        if !mismatched.is_empty() {
            println!(
                "  {} slot(s) whose SeqNo did not match their ring position, first few:",
                mismatched.len()
            );
            for (expected, actual) in mismatched.iter().take(6) {
                println!("    ring position {expected} holds SeqNo {actual}");
            }
            println!("  -> So a slot's SeqNo is not its ring position. Whatever Head counts, it");
            println!("     is not what indexes the slots -- which is why the first run saw Head");
            println!("     advance and reported nothing.");
        }
        if rumble.is_empty() {
            println!("  no output reports decoded -- trigger vibration in a game while this");
            println!("  runs to find out whether rumble is reachable without their C# event.");
        } else {
            println!("  {} output report(s):", rumble.len());
            for (source, report_id, data) in rumble.iter().take(8) {
                let head: Vec<String> = data.iter().take(8).map(|b| format!("{b:02x}")).collect();
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
    println!("  Then: does the pad survive PadForge exiting? That decides whether this");
    println!("  is 'run a helper at startup' or 'supervise a .NET service'.");
}

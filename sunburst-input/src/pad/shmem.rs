// SPDX-License-Identifier: GPL-2.0-or-later

//! The driver's shared-memory contract — how a report reaches the driver.
//!
//! Cleaned up from `tools/probe-windows/src/hidmaestro.rs`. The injector **owns**
//! the sections (it `CreateFileMapping`s them; the driver opens them once its
//! device node exists), writes each input report under a seqlock, and rings a
//! named event the driver's worker waits on. Output reports (the game's rumble /
//! adaptive-trigger / LED writes) arrive in a 64-slot ring the injector drains.
//!
//! The layout constants and the pure seqlock / ring-parsing helpers are
//! cross-platform and host-tested; only the mapping, event and I/O touch Win32 and
//! are `#[cfg(windows)]`. Its runtime needs the driver (the held-back `device.rs`
//! creates the node), so the Win32 half is exercised on the box, not here.

// ── Input section layout (must agree with the driver's SHARED_INPUT) ──────────

/// Seqlock counter: odd while a write is in flight, even when settled.
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
pub const INPUT_SIZE: usize = IN_EXT_DATA + IN_EXT_CAPACITY;

// Every offset checks the one before it, so the layout cannot drift silently.
const _: () = assert!(IN_SEQNO == 0);
const _: () = assert!(IN_DATA == IN_DATA_SIZE + 4);
const _: () = assert!(IN_GIP_DATA == IN_DATA + IN_DATA_CAPACITY);
const _: () = assert!(IN_EXT_SIZE == IN_GIP_DATA + IN_GIP_LENGTH);
const _: () = assert!(IN_EXT_DATA == IN_EXT_SIZE + 4);
const _: () = assert!(INPUT_SIZE == 362);

// ── Output ring layout ────────────────────────────────────────────────────────

/// `Head` (a u32 write count) sits at offset 0; the injector keys off slot
/// sequence numbers instead, so the count itself is not read.
const OUT_HEADER_SIZE: usize = 8;
const OUT_RING_SLOTS: usize = 64;
const OUT_SLOT_SIZE: usize = 4 + 1 + 1 + 2 + 256;
const OUT_SLOT_SEQNO: usize = 0;
const OUT_SLOT_SOURCE: usize = 4;
const OUT_SLOT_REPORT_ID: usize = 5;
const OUT_SLOT_SIZE_OFF: usize = 6;
const OUT_SLOT_DATA: usize = 8;
/// Total output section size.
pub const OUTPUT_SIZE: usize = OUT_HEADER_SIZE + OUT_RING_SLOTS * OUT_SLOT_SIZE;

const _: () = assert!(OUT_SLOT_SOURCE == OUT_SLOT_SEQNO + 4);
const _: () = assert!(OUT_SLOT_REPORT_ID == OUT_SLOT_SOURCE + 1);
const _: () = assert!(OUT_SLOT_SIZE_OFF == OUT_SLOT_REPORT_ID + 1);
const _: () = assert!(OUT_SLOT_DATA == OUT_SLOT_SIZE_OFF + 2);
const _: () = assert!(OUT_SLOT_SIZE == OUT_SLOT_DATA + 256);
const _: () = assert!(OUTPUT_SIZE == 16904);

/// The even value a seqlock write starts from: the current counter rounded up to
/// even, so a write in progress (odd) is never rewound.
///
/// The spike's first version computed the odd/even values independently from the
/// value it read, so reading an odd counter mid-write moved it *backwards*.
/// Rounding up first makes the write monotonic and always settles even.
pub fn seqlock_start(base: u32) -> u32 {
    if base.is_multiple_of(2) {
        base
    } else {
        base.wrapping_add(1)
    }
}

/// One decoded output-ring slot: a report the game wrote to the pad.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputReport {
    pub seq: u32,
    pub source: u8,
    pub report_id: u8,
    pub data: Vec<u8>,
}

/// Section name for a pad's input section.
pub fn input_name(index: u8) -> String {
    format!("Global\\HIDMaestroInput{index}")
}
/// Section name for a pad's output ring.
pub fn output_name(index: u8) -> String {
    format!("Global\\HIDMaestroOutput{index}")
}
/// Name of the event the driver's worker waits on for new input.
pub fn event_name(index: u8) -> String {
    format!("Global\\HIDMaestroInputEvent{index}")
}

#[cfg(windows)]
pub use win::{Doorbell, Section, drain_output, submit};

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE,
        UnmapViewOfFile,
    };
    use windows::Win32::System::Threading::{CreateEventW, SetEvent};
    use windows::core::HSTRING;

    use super::*;

    /// A named auto-reset event the driver's per-device worker waits on. Created
    /// by us; the driver opens it.
    pub struct Doorbell(HANDLE);

    impl Doorbell {
        /// Create the named input event for a pad.
        pub fn create(index: u8) -> Result<Doorbell, String> {
            Doorbell::create_named(&event_name(index))
        }

        /// Create an auto-reset event by explicit name (tests use a non-`Global\`
        /// name so no privilege is needed).
        fn create_named(name: &str) -> Result<Doorbell, String> {
            // SAFETY: a NUL-terminated wide name; auto-reset, initially unset.
            let handle = unsafe { CreateEventW(None, false, false, &HSTRING::from(name)) }
                .map_err(|e| format!("CreateEventW({name}): {e}"))?;
            Ok(Doorbell(handle))
        }

        pub fn ring(&self) {
            // SAFETY: a live auto-reset event handle.
            unsafe {
                let _ = SetEvent(self.0);
            }
        }
    }

    impl Drop for Doorbell {
        fn drop(&mut self) {
            // SAFETY: ours, closed exactly once.
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(self.0);
            }
        }
    }

    /// One mapped, injector-owned section.
    pub struct Section {
        handle: HANDLE,
        base: *mut u8,
        len: usize,
    }

    impl Section {
        /// Create (or open, if it already exists) a named section of `len` bytes,
        /// backed by the pagefile, mapped read/write.
        pub fn create(name: &str, len: usize) -> Result<Section, String> {
            // SAFETY: `name` is a NUL-terminated wide string; INVALID_HANDLE_VALUE
            // requests a pagefile-backed mapping of `len` bytes.
            let handle = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    None,
                    PAGE_READWRITE,
                    0,
                    len as u32,
                    &HSTRING::from(name),
                )
            }
            .map_err(|e| format!("CreateFileMappingW({name}): {e}"))?;

            // SAFETY: `handle` is a live section handle and `len` is within it.
            let view = unsafe { MapViewOfFile(handle, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, len) };
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
            // Aligned u32 fields (the seqlock counters) get a single volatile
            // read. IN_EXT_SIZE sits at a 2-misaligned offset in the fixed
            // layout, where `read_volatile::<u32>` is UB and aborts under the
            // Windows debug misalignment check, so read it byte-wise like the
            // u16 fields — it is a seqlock-guarded size, not a counter, so a torn
            // read only forces a retry.
            if (self.base as usize + offset).is_multiple_of(4) {
                // SAFETY: aligned and within the view; volatile because another
                // process writes it.
                unsafe { std::ptr::read_volatile(self.base.add(offset).cast::<u32>()) }
            } else {
                let mut bytes = [0u8; 4];
                self.read_bytes(offset, &mut bytes);
                u32::from_le_bytes(bytes)
            }
        }

        fn read_u16(&self, offset: usize) -> u16 {
            let mut bytes = [0u8; 2];
            self.read_bytes(offset, &mut bytes);
            u16::from_le_bytes(bytes)
        }

        fn write_u32(&self, offset: usize, value: u32) {
            debug_assert!(offset + 4 <= self.len);
            // See read_u32: aligned counters get a single volatile write; the
            // lone misaligned size field (IN_EXT_SIZE) is written byte-wise to
            // avoid a misaligned u32 store, safe under the seqlock.
            if (self.base as usize + offset).is_multiple_of(4) {
                // SAFETY: aligned and within the view, mapped writable.
                unsafe { std::ptr::write_volatile(self.base.add(offset).cast::<u32>(), value) }
            } else {
                self.write_bytes(offset, &value.to_le_bytes());
            }
        }

        fn write_bytes(&self, offset: usize, bytes: &[u8]) {
            debug_assert!(offset + bytes.len() <= self.len);
            // SAFETY: destination range inside the view.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.base.add(offset), bytes.len())
            }
        }

        fn read_bytes(&self, offset: usize, out: &mut [u8]) {
            debug_assert!(offset + out.len() <= self.len);
            // SAFETY: source range inside the view.
            unsafe {
                std::ptr::copy_nonoverlapping(self.base.add(offset), out.as_mut_ptr(), out.len())
            }
        }
    }

    impl Drop for Section {
        fn drop(&mut self) {
            // SAFETY: `base` came from MapViewOfFile; unmapped and closed once.
            unsafe {
                let _ =
                    UnmapViewOfFile(windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: self.base.cast::<c_void>(),
                    });
                let _ = windows::Win32::Foundation::CloseHandle(self.handle);
            }
        }
    }

    /// Write one input frame under the seqlock, then ring the doorbell.
    ///
    /// `main` is the legacy/descriptor report (goes to `Data`); `gip` is the
    /// 14-byte XUSB buffer for an Xbox pad; `ext` is the armed vendor-blob report
    /// (goes to `ExtData`, with its size hinted so the driver emits it verbatim).
    pub fn submit(
        input: &Section,
        doorbell: &Doorbell,
        main: &[u8],
        gip: Option<&[u8]>,
        ext: Option<&[u8]>,
    ) {
        let start = seqlock_start(input.read_u32(IN_SEQNO));
        input.write_u32(IN_SEQNO, start.wrapping_add(1)); // odd: write in flight

        let n = main.len().min(IN_DATA_CAPACITY);
        input.write_bytes(IN_DATA, &main[..n]);
        input.write_u32(IN_DATA_SIZE, n as u32);

        if let Some(gip) = gip {
            let g = gip.len().min(IN_GIP_LENGTH);
            input.write_bytes(IN_GIP_DATA, &gip[..g]);
        }
        match ext {
            Some(ext) => {
                let e = ext.len().min(IN_EXT_CAPACITY);
                input.write_bytes(IN_EXT_DATA, &ext[..e]);
                input.write_u32(IN_EXT_SIZE, e as u32);
            }
            None => input.write_u32(IN_EXT_SIZE, 0),
        }

        input.write_u32(IN_SEQNO, start.wrapping_add(2)); // even: settled
        doorbell.ring();
    }

    /// Collect output reports newer than `seen`, scanning every ring slot. Returns
    /// the reports and the highest sequence observed (feed it back next call).
    pub fn drain_output(output: &Section, seen: u32) -> (Vec<OutputReport>, u32) {
        let mut found = Vec::new();
        let mut highest = seen;
        for position in 0..OUT_RING_SLOTS {
            let slot = OUT_HEADER_SIZE + position * OUT_SLOT_SIZE;
            let slot_seq = output.read_u32(slot + OUT_SLOT_SEQNO);
            if slot_seq == 0 || slot_seq <= seen {
                continue;
            }
            let mut one = [0u8; 1];
            output.read_bytes(slot + OUT_SLOT_SOURCE, &mut one);
            let source = one[0];
            output.read_bytes(slot + OUT_SLOT_REPORT_ID, &mut one);
            let report_id = one[0];
            let size = usize::from(output.read_u16(slot + OUT_SLOT_SIZE_OFF)).min(256);
            let mut data = vec![0u8; size];
            output.read_bytes(slot + OUT_SLOT_DATA, &mut data);
            highest = highest.max(slot_seq);
            found.push(OutputReport {
                seq: slot_seq,
                source,
                report_id,
                data,
            });
        }
        (found, highest)
    }

    // Windows-only integration tests: real file-mapping I/O with no driver and no
    // GPU. Run by the `windows` CI job (and locally on the box), which is the only
    // place they compile. Test-scoped, non-`Global\` names → no privilege needed.
    #[cfg(test)]
    mod win_tests {
        use super::*;

        fn test_name(suffix: &str) -> String {
            format!("Local\\sunburst-shmem-test-{}-{suffix}", std::process::id())
        }

        #[test]
        fn a_report_round_trips_through_an_input_section_under_the_seqlock() {
            let section = Section::create(&test_name("in"), INPUT_SIZE).expect("create input");
            let doorbell = Doorbell::create_named(&test_name("evt")).expect("create event");

            let report = [0x01u8, 0x02, 0x03, 0x04, 0x05];
            let gip = [0xAAu8; 14];
            submit(&section, &doorbell, &report, Some(&gip), None);

            assert!(
                section.read_u32(IN_SEQNO).is_multiple_of(2),
                "seqlock settled even"
            );
            assert_eq!(section.read_u32(IN_DATA_SIZE), report.len() as u32);
            let mut back = [0u8; 5];
            section.read_bytes(IN_DATA, &mut back);
            assert_eq!(back, report);
            let mut gback = [0u8; 14];
            section.read_bytes(IN_GIP_DATA, &mut gback);
            assert_eq!(gback, gip);
            assert_eq!(section.read_u32(IN_EXT_SIZE), 0, "no extended report");
        }

        #[test]
        fn an_extended_report_sets_the_ext_region_and_size() {
            let section = Section::create(&test_name("in2"), INPUT_SIZE).expect("create");
            let doorbell = Doorbell::create_named(&test_name("evt2")).expect("event");
            let ext = [0x31u8, 0x40, 0x00, 0xAB];
            submit(&section, &doorbell, &[], None, Some(&ext));
            assert_eq!(section.read_u32(IN_EXT_SIZE), ext.len() as u32);
            let mut back = [0u8; 4];
            section.read_bytes(IN_EXT_DATA, &mut back);
            assert_eq!(back, ext);
        }

        #[test]
        fn drain_reads_new_output_slots_and_skips_seen() {
            let section = Section::create(&test_name("out"), OUTPUT_SIZE).expect("create output");
            // Write a synthetic slot the way the driver would (position 0, seq 1).
            let slot = OUT_HEADER_SIZE;
            section.write_u32(slot + OUT_SLOT_SEQNO, 1);
            section.write_bytes(slot + OUT_SLOT_SOURCE, &[0x02]);
            section.write_bytes(slot + OUT_SLOT_REPORT_ID, &[0x31]);
            section.write_bytes(slot + OUT_SLOT_SIZE_OFF, &3u16.to_le_bytes());
            section.write_bytes(slot + OUT_SLOT_DATA, &[0xDE, 0xAD, 0xBE]);

            let (reports, highest) = drain_output(&section, 0);
            assert_eq!(highest, 1);
            assert_eq!(
                reports,
                vec![OutputReport {
                    seq: 1,
                    source: 0x02,
                    report_id: 0x31,
                    data: vec![0xDE, 0xAD, 0xBE],
                }]
            );
            // Already seen → nothing new.
            let (again, _) = drain_output(&section, 1);
            assert!(again.is_empty());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seqlock_rounds_up_to_even() {
        assert_eq!(seqlock_start(0), 0);
        assert_eq!(seqlock_start(2), 2);
        // An odd counter (a write in flight) rounds up, never back.
        assert_eq!(seqlock_start(1), 2);
        assert_eq!(seqlock_start(7), 8);
        assert_eq!(seqlock_start(u32::MAX), 0, "wraps rather than rewinds");
    }

    #[test]
    fn section_names_are_per_index() {
        assert_eq!(input_name(0), "Global\\HIDMaestroInput0");
        assert_eq!(output_name(3), "Global\\HIDMaestroOutput3");
        assert_eq!(event_name(1), "Global\\HIDMaestroInputEvent1");
    }
}

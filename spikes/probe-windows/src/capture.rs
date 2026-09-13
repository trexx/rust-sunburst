// SPDX-License-Identifier: GPL-2.0-or-later

//! What a capture run produced, independent of which NvFBC interface produced it.
//!
//! `NvFBCFrameGrabInfo` is shared by every ToX interface, and the measurement is
//! meant to be comparable across them and against Desktop Duplication, so the
//! result type and its reporting live here rather than in whichever module got
//! written first.

use crate::nvfbc::result_name;

/// `NvFBCFrameGrabInfo`, 0x70 layout.
///
/// Differs from the 0x50 one: `bIsHDR`, `bReservedBit1`, `bReservedBits:30` and
/// `dwWaitModeUsed` were added, and `dwReserved2` shrank to 11.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct FrameGrabInfo {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) buffer_width: u32,
    reserved: u32,
    overlay_active: i32,
    pub(crate) must_recreate: i32,
    first_buffer: i32,
    hw_mouse_visible: i32,
    protected_content: i32,
    pub(crate) driver_internal_error: u32,
    stereo_on: i32,
    igpu_capture: i32,
    source_pid: u32,
    reserved3: u32,
    /// `bIsHDR:1`, `bReservedBit1:1`, `bReservedBits:30`.
    pub(crate) flags: u32,
    pub(crate) wait_mode_used: u32,
    reserved2: [u32; 11],
}

pub(crate) const GRAB_INFO_SIZE: usize = size_of::<FrameGrabInfo>();
const _: () = assert!(GRAB_INFO_SIZE == 108);
const _: () = assert!(std::mem::offset_of!(FrameGrabInfo, flags) == 56);
const _: () = assert!(std::mem::offset_of!(FrameGrabInfo, wait_mode_used) == 60);

impl FrameGrabInfo {
    pub(crate) fn is_hdr(&self) -> bool {
        self.flags & 1 != 0
    }
    /// `dwDriverInternalError`: 0 or the sentinel both mean "nothing to report".
    pub(crate) fn driver_ok(&self) -> bool {
        self.driver_internal_error == 0 || self.driver_internal_error == 0xFBCA_11F9
    }
}

/// One capture run's results.
pub struct Capture {
    pub blocking: bool,
    pub setup_result: i32,
    pub grabs: u32,
    pub failures: u32,
    pub unique: usize,
    pub elapsed_ns: u64,
    pub p50_ns: u64,
    pub p99_ns: u64,
    pub width: u32,
    pub height: u32,
    pub is_hdr: bool,
    pub blocking_grabs: u32,
    pub driver_errors: u32,
}

impl Capture {
    pub fn fps(&self) -> f64 {
        if self.elapsed_ns == 0 {
            return 0.0;
        }
        f64::from(self.grabs) * 1e9 / self.elapsed_ns as f64
    }
}

/// Hash a sample of the frame, to tell a new frame from a repeat.
///
/// Deliberately a subsample: a 4K ARGB buffer is ~33 MB, and hashing all of it
/// per grab would cost more than the grab and swamp the number being measured.
/// ~4096 spread bytes is plenty to distinguish frames without becoming the
/// measurement itself. Runs outside the timed region regardless.
pub(crate) fn sample_hash(buffer: *const u8, len: usize) -> u64 {
    if buffer.is_null() || len == 0 {
        return 0;
    }
    let stride = (len / 4096).max(1);
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut offset = 0;
    while offset < len {
        // SAFETY: `offset < len` and the buffer is at least `len` bytes, owned
        // by NvFBC for the life of the session.
        let byte = unsafe { *buffer.add(offset) };
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        offset += stride;
    }
    hash
}

/// Print one capture run.
pub fn report(label: &str, capture: &Capture) {
    if capture.setup_result != 0 {
        println!(
            "  {label}: SetUp failed -- {}",
            result_name(capture.setup_result)
        );
        return;
    }
    println!(
        "  {label}: {}x{}, {} grabs ({} failed)",
        capture.width, capture.height, capture.grabs, capture.failures
    );
    println!(
        "      {:.1} fps, {} unique frame(s), per-grab p50 {:.2}ms p99 {:.2}ms",
        capture.fps(),
        capture.unique,
        capture.p50_ns as f64 / 1e6,
        capture.p99_ns as f64 / 1e6,
    );
    // Only remarkable when NOWAIT was asked for; in blocking mode a blocked grab
    // is the point.
    if capture.blocking_grabs > 0 && !capture.blocking {
        println!(
            "      dwWaitModeUsed non-zero on {} of {} grabs, despite NOWAIT.",
            capture.blocking_grabs, capture.grabs
        );
        println!("      Treat with suspicion: this field arrived in a struct generation");
        println!("      newer than the setup params this driver accepts, so it may not be");
        println!("      populated the way the 7.1 header describes. NOWAIT does take effect");
        println!("      -- most grabs return a repeat frame -- so the field, not the flag,");
        println!("      is what looks wrong.");
    }
    if capture.driver_errors > 0 {
        println!(
            "      {} grab(s) reported dwDriverInternalError",
            capture.driver_errors
        );
    }
}

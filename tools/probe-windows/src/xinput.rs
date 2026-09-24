// SPDX-License-Identifier: GPL-2.0-or-later

//! Timing gamepad state changes through XInput, to price USB/IP.
//!
//! # Why not raw HID
//!
//! The first attempt read HID input reports directly, and an Xbox controller
//! returned nothing at all in ten seconds. That is not a fault: **on Windows an
//! Xbox pad does not deliver input over raw HID.** The XUSB driver claims the
//! device and games read it through XInput, so a `ReadFile` on its HID
//! collection waits forever for reports that were never going to come. PadForge's
//! stack also includes HidHide, whose whole purpose is making physical devices
//! invisible to other processes, which would hide them from that path anyway.
//!
//! XInput is both the layer that works and the layer that matters: it is how a
//! game actually sees the pad, so its timing is the timing that counts.
//!
//! # What is measured
//!
//! `XINPUT_STATE.dwPacketNumber` increments **only when the pad's state
//! changes**, so polling fast and watching that counter gives the arrival time of
//! each change without needing the device to be readable. The gap between
//! arrivals is the pad's effective report interval.
//!
//! Run it with the adapter connected directly, then again with it forwarded over
//! USB/IP, and compare. **p50 is the pad's own cadence; the tail is what the link
//! adds.** If p50 itself moves, the transport is rate-limiting rather than
//! jittering, which is a different and worse problem.
//!
//! The poll rate bounds the resolution: nothing below one poll interval can be
//! observed, so this measures arrival spacing rather than absolute latency.

use sunburst_core::instr::clock;
use windows::Win32::UI::Input::XboxController::{XINPUT_STATE, XInputGetState};

/// `ERROR_SUCCESS` from XInput.
const XINPUT_OK: u32 = 0;
/// Four is all XInput has ever supported, and the Xbox Wireless Adapter's
/// capacity is the same four.
const MAX_PADS: u32 = 4;

struct Pad {
    index: u32,
    gaps: Vec<u64>,
    changes: u64,
    polls: u64,
}

fn percentile(sorted: &[u64], pct: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[(sorted.len() * pct / 100).min(sorted.len() - 1)]
}

fn connected() -> Vec<u32> {
    (0..MAX_PADS)
        .filter(|index| {
            let mut state = XINPUT_STATE::default();
            // SAFETY: `state` is a valid out parameter and the index is in range.
            unsafe { XInputGetState(*index, &mut state) == XINPUT_OK }
        })
        .collect()
}

pub fn run() {
    println!("== XInput state-change intervals ==");
    println!("  For pricing USB/IP against the Xbox Wireless Adapter. Run with the pad");
    println!("  connected directly, then again with it forwarded, and use it the same");
    println!("  way both times -- this measures when changes arrive, so a still pad");
    println!("  measures nothing.");
    println!();

    let present = connected();
    if present.is_empty() {
        println!("  no XInput pads connected (slots 0..{MAX_PADS} all empty).");
        println!();
        println!("  -> If a pad is plugged in and this is still empty, something is claiming");
        println!("     it exclusively. PadForge's stack includes HidHide, which exists to");
        println!("     hide physical devices from other processes -- close PadForge and");
        println!("     retry before concluding anything about the hardware.");
        return;
    }
    println!("  pads in slots: {present:?}");
    println!();
    println!("  >>> Hold a stick OFF-CENTRE for the whole 10s, do not just press buttons.");
    println!("  >>> An off-centre stick jitters in its analog values, so the pad reports");
    println!("  >>> continuously at its full rate. A pad at rest reports nothing, and every");
    println!("  >>> pause becomes a long gap that looks like link jitter but is just hands.");

    let mut pads: Vec<Pad> = present
        .iter()
        .map(|index| Pad {
            index: *index,
            gaps: Vec::new(),
            changes: 0,
            polls: 0,
        })
        .collect();
    let mut last_packet = vec![0u32; pads.len()];
    let mut last_change = vec![None::<u64>; pads.len()];
    let mut primed = vec![false; pads.len()];

    let deadline = clock::now() + clock::ticks_per_sec() * 10;
    while clock::now() < deadline {
        for (slot, pad) in pads.iter_mut().enumerate() {
            let mut state = XINPUT_STATE::default();
            // SAFETY: `state` is a valid out parameter.
            if unsafe { XInputGetState(pad.index, &mut state) } != XINPUT_OK {
                continue;
            }
            pad.polls += 1;
            if !primed[slot] {
                last_packet[slot] = state.dwPacketNumber;
                primed[slot] = true;
                continue;
            }
            if state.dwPacketNumber == last_packet[slot] {
                continue;
            }
            last_packet[slot] = state.dwPacketNumber;
            pad.changes += 1;

            let now = clock::now();
            if let Some(previous) = last_change[slot] {
                pad.gaps.push(clock::ticks_to_ns(now - previous));
            }
            last_change[slot] = Some(now);
        }
        // Spin rather than sleep. `sleep(1ms)` actually landed around 1.4ms on
        // Windows' default timer granularity -- 733Hz against an 8ms signal, so
        // every interval carried ~17% quantisation. A 10s probe can afford a
        // core, and the achieved rate is reported so the resolution is visible
        // rather than assumed.
        std::hint::spin_loop();
    }

    let elapsed = clock::ticks_to_ns(clock::now() - (deadline - clock::ticks_per_sec() * 10));
    let seconds = elapsed as f64 / 1e9;

    println!();
    for pad in &mut pads {
        pad.gaps.sort_unstable();
        if pad.gaps.is_empty() {
            println!(
                "  slot {}: {} polls, {} changes -- nothing to time. Use the pad while it runs.",
                pad.index, pad.polls, pad.changes
            );
            continue;
        }
        let p50 = percentile(&pad.gaps, 50);
        let p99 = percentile(&pad.gaps, 99);
        let max = pad.gaps[pad.gaps.len() - 1];
        println!(
            "  slot {}: p50 {:.2}ms  p99 {:.2}ms  max {:.2}ms   ({} changes over {} polls)",
            pad.index,
            p50 as f64 / 1e6,
            p99 as f64 / 1e6,
            max as f64 / 1e6,
            pad.gaps.len(),
            pad.polls,
        );
        // Microseconds: spinning reaches hundreds of kHz, where a millisecond
        // figure just reads "0.00" and looks broken.
        println!(
            "           poll rate {:.0}kHz, so resolution is ~{:.1}us",
            pad.polls as f64 / seconds / 1e3,
            seconds * 1e6 / pad.polls.max(1) as f64,
        );

        // How much of the run was continuous. A stick held off-centre reports at
        // the pad's full rate; every pause shows up as an outlier, and without
        // this the tail reads as link jitter when it is just idle time.
        let near = pad
            .gaps
            .iter()
            .filter(|g| **g <= p50.saturating_mul(2))
            .count();
        let continuity = near as f64 * 100.0 / pad.gaps.len() as f64;
        println!("           {continuity:.0}% of gaps within 2x p50");
        if continuity < 90.0 {
            println!("           -> The pad was not moving continuously, so p99 and max are");
            println!("              mostly pauses rather than the link. Only p50 is usable");
            println!("              from this run; hold a stick off-centre and repeat.");
        }
        if pad.gaps.len() < 100 {
            println!("           (under 100 samples -- p99 is the max here, not a percentile)");
        }
    }

    println!();
    println!("  -> Compare direct against forwarded. p50 is the pad's own cadence and");
    println!("     should not move; the tail is what USB/IP adds -- but only if continuity");
    println!("     is high in both runs, otherwise the tail is just idle time.");
    println!();
    println!("     p50 near 8ms is the expected 125Hz for an Xbox pad, which doubles as a");
    println!("     check that the measurement is sane before any comparison is drawn.");
}

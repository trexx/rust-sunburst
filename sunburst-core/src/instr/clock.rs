// SPDX-License-Identifier: GPL-2.0-or-later

//! Monotonic tick source.
//!
//! `cfg`-selected free functions rather than a trait. A trait would put virtual
//! dispatch on the frame path for the sake of testability, and the project rule
//! is the other way round: where a test needs to control time, the timestamp is
//! passed in as a parameter instead.
//!
//! Ticks are deliberately not nanoseconds on Windows. `QueryPerformanceCounter`
//! runs at whatever `QueryPerformanceFrequency` reports (10 MHz on current
//! hardware), and normalising per sample would put a multiply and a divide on
//! the hot path to no purpose — [`ticks_to_ns`] runs in the drain thread, once
//! per sample, off the frame path.

/// Raw monotonic tick. Only meaningful relative to another tick from the same
/// machine — there is no shared epoch across the link.
#[inline(always)]
pub fn now() -> u64 {
    imp::now()
}

/// Ticks per second, queried once and cached.
///
/// Never call this on the frame path; it exists so the drain thread can convert.
pub fn ticks_per_sec() -> u64 {
    use std::sync::OnceLock;
    static FREQ: OnceLock<u64> = OnceLock::new();
    *FREQ.get_or_init(imp::frequency)
}

/// Convert a tick *delta* to nanoseconds.
///
/// Splits the multiply rather than computing `ticks * 1_000_000_000 / freq`,
/// which overflows `u64` at about 31 minutes of delta on a 10 MHz QPC. That is
/// not a duration any stage produces, but the frame table can hold a timestamp
/// across an idle period and subtract it later, and a silent wrap there would
/// land in the p99 column as a plausible-looking small number.
#[inline]
pub fn ticks_to_ns(ticks: u64) -> u64 {
    let freq = ticks_per_sec();
    let secs = ticks / freq;
    let rem = ticks % freq;
    secs * 1_000_000_000 + (rem * 1_000_000_000) / freq
}

#[cfg(windows)]
mod imp {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn QueryPerformanceCounter(count: *mut i64) -> i32;
        fn QueryPerformanceFrequency(freq: *mut i64) -> i32;
    }

    #[inline(always)]
    pub fn now() -> u64 {
        let mut t: i64 = 0;
        // SAFETY: `t` is a valid, aligned, writable i64. QPC is documented never
        // to fail on Windows XP or later, and writes only through this pointer.
        unsafe { QueryPerformanceCounter(&mut t) };
        t as u64
    }

    pub fn frequency() -> u64 {
        let mut f: i64 = 0;
        // SAFETY: as above; QPF has the same contract and the same guarantee.
        unsafe { QueryPerformanceFrequency(&mut f) };
        // A zero frequency would make every conversion divide by zero. QPF
        // cannot return zero on a supported OS, but the fallback costs nothing
        // and turns an impossible panic into an obviously-wrong number.
        if f > 0 { f as u64 } else { 10_000_000 }
    }
}

#[cfg(unix)]
mod imp {
    #[inline(always)]
    pub fn now() -> u64 {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid, aligned, writable timespec and CLOCK_MONOTONIC
        // is always supported on Linux and Android. Resolved through the vDSO, so
        // this is not a syscall in practice.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
    }

    pub fn frequency() -> u64 {
        // clock_gettime already reports nanoseconds, so ticks are nanoseconds
        // and ticks_to_ns is the identity.
        1_000_000_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_monotonic() {
        let a = now();
        let b = now();
        assert!(b >= a, "clock went backwards: {a} then {b}");
    }

    #[test]
    fn frequency_is_sane() {
        let f = ticks_per_sec();
        assert!(f >= 1_000_000, "suspiciously coarse clock: {f} Hz");
    }

    #[test]
    fn ticks_to_ns_round_trips_one_second() {
        assert_eq!(ticks_to_ns(ticks_per_sec()), 1_000_000_000);
    }

    #[test]
    fn ticks_to_ns_survives_a_long_delta() {
        // One hour. The naive `ticks * 1e9 / freq` formulation overflows well
        // before this at a 10 MHz QPC frequency and would return a small,
        // entirely believable number.
        let hour = ticks_per_sec() * 3600;
        assert_eq!(ticks_to_ns(hour), 3_600 * 1_000_000_000);
    }
}

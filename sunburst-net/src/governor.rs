// SPDX-License-Identifier: GPL-2.0-or-later

//! The capture frame-rate governor: at most one encode per client frame
//! interval, without ever losing the last frame of a motion.
//!
//! The server's desktop usually runs faster than the TV (CLAUDE.md recommends a
//! high-refresh desktop), so the capture thread sees more frames than the client
//! can show and must skip some. Which ones matters:
//!
//! - **Admission is the long-standing rule, unchanged**: encode a frame if the
//!   client's next slot has opened (`now >= next_slot`), then move the slot one
//!   interval on (`next_slot = max(next_slot + interval, now)`). Simulated over
//!   vsync-snapped game cadences, every change to this rule that was tried —
//!   tolerance windows, re-anchoring, a token bucket — made the primary case
//!   (a 60 fps game into a 59.94 / 60 Hz TV) worse: bursts, skipped game frames,
//!   or ~20 ms of added p99 latency. So it stays.
//! - **A frame that arrives before its slot is held, not thrown away.** The old
//!   loop discarded it and went back to waiting. On a desktop that then goes
//!   still — a menu closing, the last keystroke — nothing else ever arrives
//!   (capture only yields on change), so that final frame was never encoded:
//!   33–56 % of motions at 90–240 Hz desktops left the TV on a stale frame. A
//!   held frame is superseded by any newer one, and flushed only when motion has
//!   **stopped** — when the next frame is overdue at the content's own cadence.
//!
//! The flush deadline is what makes that free. `held_at + max(interval,
//! 2·gap + 1 ms)`, with `gap` a moving average of recent inter-arrival times,
//! is late enough that steady content always supersedes the held frame first
//! (so steady-state admission is exactly the old rule's — including one missed
//! vsync in a game), and early enough to land the final frame within about one
//! content interval. Flushing one *client* interval after the slot instead
//! raced the next frame of a 60 fps game and was measurably worse.
//!
//! **Content much faster than the client is resampled on a fixed grid.** At an
//! integer ratio — a 120 Hz desktop into a 60 Hz TV, 240 into 60 or 120 — the
//! desktop's frames land right on the client's slot boundaries, and "admit once
//! the slot is open, re-anchor to the arrival" flips on sub-millisecond jitter:
//! it samples motion as 25/8 ms steps instead of 16.7/16.7, 19–38 visible
//! judder events a second where an ideal resampler has none. So while the
//! content cadence is clearly faster than the client (entering below 0.70 of
//! the interval, leaving above 0.85 — hysteresis, so a game's wandering frame
//! rate does not flip-flop), the slots stay on a fixed grid and each slot takes
//! the frame nearest it: admitted if it is no more than half a content interval
//! early. Content at or below the client rate never enters that mode, so the
//! primary case keeps the old rule exactly. Measured in simulation against an
//! offline best-pick oracle: 120→60 judder 19.2 → 0.1 events/s, 240→120
//! 38.5 → 0.1, identical admission at 58/60/62→60, 60→59.94 and 117→120, no
//! added latency.
//!
//! Pure logic, clock as an argument (u64 nanoseconds), integer-only — the same
//! shape as [`crate::send::Pacer`], so it is host-tested here and the Windows
//! capture loop only feeds it timestamps.

/// Inter-arrival gaps at least this long are idle time, not content cadence,
/// and do not move the cadence estimate.
const IDLE_GAP_NS: u64 = 100_000_000;
/// Slack added to twice the cadence, so a game that misses one vsync is not
/// mistaken for motion stopping.
const FLUSH_SLACK_NS: u64 = 1_000_000;
/// Grid mode is entered when the content cadence drops below this fraction of
/// the client interval, and left when it rises above the second (tenths / 20ths
/// so the comparison stays integer).
const GRID_ENTER_TENTHS: u128 = 7;
const GRID_LEAVE_TWENTIETHS: u128 = 17;

/// The refresh assumed when a client reports none (millihertz).
const DEFAULT_REFRESH_MHZ: u32 = 60_000;

/// The governor's frame interval for a client refreshing at `refresh_mhz`
/// (millihertz), optionally capped at `fps_cap` frames per second (0 = none).
///
/// Exact, not rounded to whole frames: a 59.94 Hz TV truncated to a 59 fps
/// governor dropped about one frame a second from a steady 60 fps game.
pub fn client_interval_ns(refresh_mhz: u32, fps_cap: u32) -> u64 {
    let refresh = if refresh_mhz == 0 {
        DEFAULT_REFRESH_MHZ
    } else {
        refresh_mhz
    };
    let interval = 1_000_000_000_000 / u64::from(refresh);
    if fps_cap > 0 {
        interval.max(1_000_000_000 / u64::from(fps_cap))
    } else {
        interval
    }
}

/// Whole frames per second for the encoder's rate header and VBV sizing:
/// the client's refresh rounded to nearest (59.94 Hz is 60), capped by
/// `fps_cap` (0 = none).
pub fn encoder_fps(refresh_mhz: u32, fps_cap: u32) -> u32 {
    let refresh = if refresh_mhz == 0 {
        DEFAULT_REFRESH_MHZ
    } else {
        refresh_mhz
    };
    let fps = refresh.saturating_add(500) / 1000;
    let fps = if fps_cap > 0 { fps.min(fps_cap) } else { fps };
    fps.max(1)
}

/// What to do with a frame that just arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admit {
    /// Encode it now.
    Encode,
    /// Convert it and keep it; encode it at [`FrameGovernor::flush_deadline`]
    /// unless a newer frame supersedes it first.
    Hold,
    /// Drop it. Only when holding is disabled — a capture backend that cannot
    /// wake the loop at a deadline (see [`FrameGovernor::new`]).
    Skip,
}

/// Admission and hold state for one capture loop.
#[derive(Clone, Debug)]
pub struct FrameGovernor {
    interval_ns: u64,
    hold_enabled: bool,
    /// When the client's next slot opens. Meaningless until `started`.
    next_slot: u64,
    started: bool,
    /// Moving average of recent inter-arrival gaps: the content's cadence.
    gap_ns: u64,
    last_arrival: Option<u64>,
    /// When the held frame must be flushed, if one is held.
    deadline: Option<u64>,
    /// Resampling on a fixed grid: content is clearly faster than the client.
    grid: bool,
}

impl FrameGovernor {
    /// A governor for a client that shows one frame per `interval_ns`.
    ///
    /// `hold_enabled` must be `false` for a capture backend whose wait ignores
    /// its timeout (NvFBC's blocking grab): the loop could never wake to flush a
    /// held frame, so early frames are skipped exactly as before.
    pub fn new(interval_ns: u64, hold_enabled: bool) -> FrameGovernor {
        let interval_ns = interval_ns.max(1);
        FrameGovernor {
            interval_ns,
            hold_enabled,
            next_slot: 0,
            started: false,
            gap_ns: interval_ns,
            last_arrival: None,
            deadline: None,
            grid: false,
        }
    }

    /// The client frame interval this governor enforces.
    pub fn interval_ns(&self) -> u64 {
        self.interval_ns
    }

    /// A captured frame arrived at `now_ns`.
    pub fn on_frame(&mut self, now_ns: u64) -> Admit {
        if let Some(last) = self.last_arrival {
            let gap = now_ns.saturating_sub(last);
            if gap < IDLE_GAP_NS {
                // EMA, alpha = 1/5.
                self.gap_ns = self.gap_ns - self.gap_ns / 5 + gap / 5;
            }
        }
        self.last_arrival = Some(now_ns);

        if !self.started {
            self.admit(now_ns);
            return Admit::Encode;
        }
        let (gap, interval) = (u128::from(self.gap_ns), u128::from(self.interval_ns));
        if self.grid && gap * 20 > interval * GRID_LEAVE_TWENTIETHS {
            self.grid = false;
        } else if !self.grid && gap * 10 < interval * GRID_ENTER_TENTHS {
            self.grid = true;
        }
        if self.grid {
            self.catch_up(now_ns);
            // The frame nearest the slot: no more than half a content interval
            // early, else the next frame will be nearer.
            if now_ns.saturating_add(self.gap_ns / 2) >= self.next_slot {
                self.next_slot = self.next_slot.saturating_add(self.interval_ns);
                self.deadline = None;
                return Admit::Encode;
            }
        } else if now_ns >= self.next_slot {
            self.admit(now_ns);
            return Admit::Encode;
        }
        if !self.hold_enabled {
            return Admit::Skip;
        }
        // Latest wins: a newer early frame replaces the held one and pushes the
        // deadline out, because motion evidently has not stopped.
        let wait = self
            .interval_ns
            .max(self.gap_ns.saturating_mul(2).saturating_add(FLUSH_SLACK_NS));
        self.deadline = Some(self.next_slot.max(now_ns.saturating_add(wait)));
        Admit::Hold
    }

    /// When the held frame must be encoded, if a frame is held.
    pub fn flush_deadline(&self) -> Option<u64> {
        self.deadline
    }

    /// The held frame was encoded at `now_ns`.
    pub fn on_flush(&mut self, now_ns: u64) {
        if self.grid {
            self.catch_up(now_ns);
            self.next_slot = self.next_slot.saturating_add(self.interval_ns);
            self.deadline = None;
        } else {
            self.admit(now_ns);
        }
    }

    /// Grid mode: step over whole slots that passed with no frame (a content
    /// stall), keeping the grid's phase.
    fn catch_up(&mut self, now_ns: u64) {
        if now_ns >= self.next_slot.saturating_add(self.interval_ns) {
            let behind = (now_ns - self.next_slot) / self.interval_ns;
            self.next_slot = self
                .next_slot
                .saturating_add(behind.saturating_mul(self.interval_ns));
        }
    }

    /// Forget everything: the capture was rebuilt, or the desktop became
    /// unavailable, and any held frame is gone with it.
    pub fn reset(&mut self) {
        *self = FrameGovernor::new(self.interval_ns, self.hold_enabled);
    }

    fn admit(&mut self, now_ns: u64) {
        self.next_slot = if self.started {
            self.next_slot.saturating_add(self.interval_ns).max(now_ns)
        } else {
            now_ns
        };
        self.started = true;
        self.deadline = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    /// A deterministic LCG — this crate has no `rand`, and the tests must be
    /// reproducible anyway.
    struct Lcg(u64);

    impl Lcg {
        fn next_f64(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }

        /// Approximately normal, mean 0, sd 1 (sum of uniforms).
        fn gauss(&mut self) -> f64 {
            (0..12).map(|_| self.next_f64()).sum::<f64>() - 6.0
        }
    }

    /// Frame arrival times (ns) for a game rendering at `game_fps` on a desktop
    /// refreshing at `desk_hz`: each game frame presents at the next vsync,
    /// with `jitter` relative frame-time noise. With `idle`, the content
    /// stops for 400 ms about every 50 frames — each stop is a motion ending.
    fn cadence(
        desk_hz: f64,
        game_fps: f64,
        secs: f64,
        jitter: f64,
        seed: u64,
        idle: bool,
    ) -> Vec<u64> {
        let mut rng = Lcg(seed);
        let vsync = 1000.0 / desk_hz;
        let frame = 1000.0 / game_fps;
        let mut out: Vec<u64> = Vec::new();
        let mut t = 10.0; // ms
        let mut last_v = -1.0;
        while t < secs * 1000.0 {
            if idle && !out.is_empty() && rng.next_f64() < 1.0 / 50.0 {
                t += 400.0;
            }
            let v = (t / vsync).ceil() * vsync;
            if v != last_v {
                // Sub-0.2 ms capture latency on top of the vsync.
                let arrive = v + (rng.gauss() * 0.2).abs();
                out.push((arrive * MS as f64) as u64);
                last_v = v;
            }
            t += frame * (1.0 + rng.gauss() * jitter);
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// The pre-fix governor, verbatim in spirit: admit when the slot is open,
    /// discard otherwise.
    fn legacy(arrivals: &[u64], interval: u64) -> Vec<u64> {
        let mut next = 0u64;
        let mut started = false;
        let mut out = Vec::new();
        for &a in arrivals {
            if !started || a >= next {
                next = if started { (next + interval).max(a) } else { a };
                started = true;
                out.push(a);
            }
        }
        out
    }

    /// Drive a governor through `arrivals` the way the capture loop does:
    /// flush a held frame whose deadline passes before the next arrival.
    /// Returns `(encoded_at, content_arrived_at)` per encode.
    fn drive(arrivals: &[u64], interval: u64) -> Vec<(u64, u64)> {
        let mut g = FrameGovernor::new(interval, true);
        let mut held: Option<u64> = None;
        let mut out = Vec::new();
        for &a in arrivals {
            if let (Some(h), Some(dl)) = (held, g.flush_deadline())
                && dl <= a
            {
                g.on_flush(dl);
                out.push((dl, h));
                // `held` is reassigned by the arrival just below.
            }
            match g.on_frame(a) {
                Admit::Encode => {
                    held = None;
                    out.push((a, a));
                }
                Admit::Hold => held = Some(a),
                Admit::Skip => unreachable!("holding is enabled"),
            }
        }
        if let (Some(h), Some(dl)) = (held, g.flush_deadline()) {
            out.push((dl, h));
        }
        out
    }

    fn interval(hz: f64) -> u64 {
        (1e9 / hz) as u64
    }

    /// Content-advance judder: consecutive encoded frames whose content times
    /// differ from the ideal step (the client interval, or the content's own
    /// step when it is slower) by more than 0.8 of a desktop vsync. At an
    /// integer ratio that is a 1.5-then-0.5-frame step — visible stutter no
    /// client can smooth, because it is in *which* moments were sampled.
    fn judder(contents: &[u64], arrivals: &[u64], client: u64, vsync: u64) -> usize {
        let mut c = contents.to_vec();
        c.sort_unstable();
        c.windows(2)
            .filter(|w| {
                let d = w[1] - w[0];
                if d > 100 * MS {
                    return false; // idle, not motion
                }
                let i = arrivals.partition_point(|&a| a <= w[0]);
                let local = arrivals.get(i).map_or(client, |&n| n - w[0]);
                let ideal = client.max(local);
                d.abs_diff(ideal) * 10 > vsync * 8
            })
            .count()
    }

    #[test]
    fn a_59_94_hz_client_keeps_its_exact_interval() {
        assert_eq!(client_interval_ns(59_940, 0), 16_683_350);
        assert_eq!(encoder_fps(59_940, 0), 60, "rounded, not truncated to 59");
        assert_eq!(client_interval_ns(60_000, 0), 16_666_666);
        assert_eq!(encoder_fps(120_000, 0), 120);
    }

    #[test]
    fn an_fps_cap_lengthens_the_interval_and_caps_the_encoder() {
        assert_eq!(client_interval_ns(120_000, 60), 16_666_666);
        assert_eq!(encoder_fps(120_000, 60), 60);
        // A cap above the refresh changes nothing.
        assert_eq!(client_interval_ns(60_000, 144), 16_666_666);
        assert_eq!(encoder_fps(59_940, 144), 60);
    }

    #[test]
    fn an_unreported_refresh_assumes_60_hz() {
        assert_eq!(client_interval_ns(0, 0), 16_666_666);
        assert_eq!(encoder_fps(0, 0), 60);
    }

    #[test]
    fn steady_game_content_is_admitted_exactly_as_before() {
        // The primary workload: a game near the client's rate, on a
        // high-refresh desktop. Nothing here may change which frames are sent.
        for (desk, game, client) in [
            (144.0, 58.0, 60.0),
            (144.0, 60.0, 60.0),
            (144.0, 62.0, 60.0),
            (144.0, 60.0, 59.94),
            (144.0, 117.0, 120.0),
            (144.0, 144.0, 120.0),
            (60.0, 60.0, 59.94),
        ] {
            let arr = cadence(desk, game, 20.0, 0.01, 7, false);
            let want = legacy(&arr, interval(client));
            let got: Vec<u64> = drive(&arr, interval(client))
                .iter()
                .map(|&(_, c)| c)
                .collect();
            assert_eq!(
                got, want,
                "{game} fps on a {desk} Hz desktop into {client} Hz changed admission"
            );
        }
    }

    #[test]
    fn integer_ratio_content_is_sampled_evenly() {
        // A 120 Hz desktop into a 60 Hz TV, and 240 into 60 / 120: the old rule
        // flipped on jitter at every slot boundary and sampled motion unevenly.
        for (desk, client) in [(120.0, 60.0), (240.0, 60.0), (240.0, 120.0)] {
            let arr = cadence(desk, desk, 20.0, 0.01, 7, false);
            let secs = 20;
            let now: Vec<u64> = drive(&arr, interval(client))
                .iter()
                .map(|&(_, c)| c)
                .collect();
            let before = legacy(&arr, interval(client));
            let vsync = interval(desk);
            let (j_now, j_before) = (
                judder(&now, &arr, interval(client), vsync),
                judder(&before, &arr, interval(client), vsync),
            );
            assert!(
                j_before > 10 * secs,
                "{desk}->{client}: the old rule should judder here, got {j_before}"
            );
            assert!(
                j_now <= secs,
                "{desk}->{client}: {j_now} judder events in {secs} s (old rule {j_before})"
            );
            let rate = now.len() as f64 / 20.0;
            assert!(
                rate <= client + 0.5,
                "{desk}->{client} encoded {rate:.1} fps"
            );
            assert!(
                rate >= client - 1.0,
                "{desk}->{client} dropped to {rate:.1} fps"
            );
        }
    }

    #[test]
    fn a_wandering_frame_rate_is_no_worse_than_before() {
        // A game whose frame rate drifts across the grid-mode thresholds must not
        // flip-flop into more judder than the old rule, or lose the rate cap.
        for (desk, lo, hi, client) in [(144.0, 50.0, 140.0, 60.0), (240.0, 100.0, 240.0, 120.0)] {
            let mut rng = Lcg(3);
            let vsync_ms = 1000.0 / desk;
            let (mut arr, mut t, mut fps, mut last_v) =
                (Vec::new(), 10.0f64, (lo + hi) / 2.0, -1.0);
            while t < 30_000.0 {
                fps = (fps + rng.gauss() * 1.5).clamp(lo, hi);
                let v = (t / vsync_ms).ceil() * vsync_ms;
                if v != last_v {
                    arr.push(((v + (rng.gauss() * 0.2).abs()) * MS as f64) as u64);
                    last_v = v;
                }
                t += 1000.0 / fps;
            }
            arr.sort_unstable();
            arr.dedup();
            let now: Vec<u64> = drive(&arr, interval(client))
                .iter()
                .map(|&(_, c)| c)
                .collect();
            let before = legacy(&arr, interval(client));
            let vsync = interval(desk);
            let (j_now, j_before) = (
                judder(&now, &arr, interval(client), vsync),
                judder(&before, &arr, interval(client), vsync),
            );
            assert!(
                j_now <= j_before + j_before / 10 + 30,
                "{desk} Hz {lo}-{hi} fps -> {client}: {j_now} vs {j_before} before"
            );
            assert!(now.len() as f64 / 30.0 <= client + 0.5);
        }
    }

    #[test]
    fn steady_high_refresh_content_adds_no_latency() {
        // Faster content than the client: frames are held and superseded all
        // the time, and a flush should essentially never fire.
        for (desk, client) in [(144.0, 60.0), (165.0, 60.0), (240.0, 120.0), (120.0, 60.0)] {
            let arr = cadence(desk, desk, 20.0, 0.01, 11, false);
            let enc = drive(&arr, interval(client));
            let flushed = enc.iter().filter(|&&(e, c)| e != c).count();
            assert!(
                flushed * 100 <= enc.len(),
                "{desk} Hz into {client} Hz flushed {flushed} of {} frames",
                enc.len()
            );
        }
    }

    #[test]
    fn the_last_frame_of_every_motion_is_encoded() {
        for (desk, game, client) in [
            (144.0, 144.0, 60.0),
            (144.0, 90.0, 60.0),
            (120.0, 120.0, 60.0),
            (165.0, 165.0, 60.0),
            (240.0, 240.0, 120.0),
        ] {
            let arr = cadence(desk, game, 40.0, 0.01, 23, true);
            let enc = drive(&arr, interval(client));
            let encoded: std::collections::HashMap<u64, u64> =
                enc.iter().map(|&(e, c)| (c, e)).collect();
            let ends: Vec<u64> = arr
                .windows(2)
                .filter(|w| w[1] - w[0] > 300 * MS)
                .map(|w| w[0])
                .chain(arr.last().copied())
                .collect();
            assert!(ends.len() > 5, "the cadence should contain motion ends");
            for end in ends {
                let at = encoded.get(&end).unwrap_or_else(|| {
                    panic!("{game} fps on {desk} Hz into {client} Hz lost a final frame")
                });
                assert!(
                    at - end <= 2 * interval(client),
                    "final frame waited {} ms",
                    (at - end) / MS
                );
            }
            // And the old governor really did lose some, or this proves nothing.
            let legacy_set: std::collections::HashSet<u64> =
                legacy(&arr, interval(client)).into_iter().collect();
            let lost_before = arr
                .windows(2)
                .filter(|w| w[1] - w[0] > 300 * MS && !legacy_set.contains(&w[0]))
                .count();
            if desk > game || desk >= 120.0 {
                assert!(lost_before > 0, "{desk}/{game}: legacy lost nothing here");
            }
        }
    }

    #[test]
    fn the_encode_rate_never_exceeds_the_client() {
        let client = interval(60.0);
        let arr = cadence(240.0, 240.0, 20.0, 0.01, 5, false);
        let enc = drive(&arr, client);
        let span = arr.last().unwrap() - arr.first().unwrap();
        let fps = enc.len() as f64 / (span as f64 / 1e9);
        assert!(fps <= 60.5, "encoded {fps:.1} fps into a 60 Hz client");
    }

    /// A governor whose next slot is 16 ms, with the grid ahead of the clock:
    /// admitted at 0 (slot = 0) and again at 10 ms (slot = max(0 + 16, 10)).
    /// Only once admissions run ahead of the grid can a frame be early.
    fn primed(hold: bool) -> FrameGovernor {
        let mut g = FrameGovernor::new(16 * MS, hold);
        assert_eq!(g.on_frame(0), Admit::Encode, "the very first frame");
        assert_eq!(g.on_frame(10 * MS), Admit::Encode);
        g
    }

    #[test]
    fn the_first_frame_after_idle_is_encoded_immediately() {
        let mut g = primed(true);
        assert_eq!(g.on_frame(12 * MS), Admit::Hold);
        g.on_flush(g.flush_deadline().expect("held"));
        assert_eq!(g.on_frame(900 * MS), Admit::Encode, "after a long idle");
    }

    #[test]
    fn an_early_frame_is_superseded_by_one_at_the_slot() {
        // The old rule: 0 opens, 16 is on time, 20 arrives with the slot at 16
        // open and pushes the next slot to 32. So 24 is early and held...
        let mut g = FrameGovernor::new(16 * MS, true);
        for t in [0, 16, 20] {
            assert_eq!(g.on_frame(t * MS), Admit::Encode);
        }
        assert_eq!(g.on_frame(24 * MS), Admit::Hold);
        let deadline = g.flush_deadline().expect("held");
        assert!(deadline >= 24 * MS + 16 * MS, "{deadline}");
        // ...and the frame at the slot replaces it; the held one is never sent.
        assert_eq!(g.on_frame(32 * MS), Admit::Encode);
        assert_eq!(g.flush_deadline(), None, "an encode clears the hold");
    }

    #[test]
    fn in_grid_mode_the_newest_early_frame_is_the_one_held() {
        // Content every 4 ms into a 16 ms client: grid mode, and a frame more
        // than half a content interval (2 ms) before its slot waits for a
        // nearer one.
        let mut g = FrameGovernor::new(16 * MS, true);
        let mut t = 0;
        while t < 400 * MS {
            g.on_frame(t);
            t += 4 * MS;
        }
        // Find the next slot by stepping until a frame is encoded, then look at
        // the frames 12 and 8 ms before the following slot.
        let mut encoded_at = t;
        while g.on_frame(encoded_at) != Admit::Encode {
            encoded_at += 4 * MS;
        }
        let slot = encoded_at + 16 * MS;
        assert_eq!(g.on_frame(slot - 12 * MS), Admit::Hold);
        assert_eq!(g.on_frame(slot - 8 * MS), Admit::Hold, "still too early");
        let deadline = g.flush_deadline().expect("held");
        assert!(
            deadline >= slot - 8 * MS + 16 * MS,
            "follows the newest: {deadline}"
        );
        assert_eq!(
            g.on_frame(slot),
            Admit::Encode,
            "the frame at the slot wins"
        );
        assert_eq!(g.flush_deadline(), None);
    }

    #[test]
    fn without_holding_early_frames_are_skipped_as_before() {
        let mut g = primed(false);
        assert_eq!(g.on_frame(12 * MS), Admit::Skip);
        assert_eq!(g.flush_deadline(), None);
        assert_eq!(g.on_frame(16 * MS), Admit::Encode);
    }

    #[test]
    fn reset_forgets_the_held_frame() {
        let mut g = primed(true);
        assert_eq!(g.on_frame(12 * MS), Admit::Hold);
        g.reset();
        assert_eq!(g.flush_deadline(), None);
        assert_eq!(g.on_frame(13 * MS), Admit::Encode, "a reset starts afresh");
    }

    #[test]
    fn survives_the_top_of_the_clock() {
        // Nothing may overflow (tests run with overflow checks) near u64::MAX,
        // in either mode, and a deadline is never set in the past.
        let mut g = FrameGovernor::new(u64::MAX / 4, true);
        let near = u64::MAX - 1_000;
        for t in [near - 50, near - 40, near - 30, near - 20, near - 10, near] {
            if g.on_frame(t) == Admit::Hold {
                assert!(g.flush_deadline().is_some_and(|d| d >= t));
            }
        }
        g.on_flush(u64::MAX);
        let _ = g.on_frame(u64::MAX);
    }
}

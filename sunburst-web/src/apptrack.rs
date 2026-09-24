// SPDX-License-Identifier: GPL-2.0-or-later

//! Whether a launched app is still running — pure, host-tested.
//!
//! "The process we started has exited" is the wrong answer for most games.
//! A launcher stub (Epic, GOG, Ubisoft, Steam for a `steam://rungameid`)
//! starts the game and exits, so watching only our child reaped the app — and
//! undid its prep — while the game was still on screen. A URI launch has no
//! child at all, and was never reaped: every later launch was refused as a
//! conflict.
//!
//! So an app is tracked one of three ways, chosen from its entry:
//!
//! - [`Tracking::Job`]: an executable we start. Everything it starts shares a
//!   job object, and the app is running while any process in the job is. A
//!   launcher that exits after starting the game is covered, since the game
//!   is in the job too.
//! - [`Tracking::Process`]: the entry names the game's own executable
//!   (`wait_process`), for a hand-off that leaves the job (a URI, or a launcher
//!   that asks the game to be started by a service). The app is starting until
//!   that process appears, and running until it has been gone for
//!   [`EXIT_DEBOUNCE_MS`]. The debounce covers a bootstrapper that restarts the
//!   game under the same name.
//! - [`Tracking::Untracked`]: a URI with no process to watch. It is never
//!   reaped, and the next launch replaces it rather than being refused.
//!
//! The Windows host gathers the observations (the job's active-process count, a
//! process snapshot); everything that decides lives here.

use serde::{Deserialize, Serialize};

use crate::config::AppEntry;

/// How long a `wait_process` app may take to appear before the launch is
/// judged to have failed — while its launcher is still running it may take
/// longer (an update, a login prompt).
pub const LAUNCH_GRACE_MS: u64 = 120_000;

/// How long a watched process must stay gone before the app counts as exited.
pub const EXIT_DEBOUNCE_MS: u64 = 5_000;

/// How an app's lifetime is judged. See the module docs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tracking {
    Job,
    Process(String),
    Untracked,
}

/// [`Tracking`]'s kind, for the UI and the API.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrackingKind {
    Job,
    Process,
    Untracked,
}

impl Tracking {
    pub fn for_entry(app: &AppEntry) -> Tracking {
        match &app.wait_process {
            Some(name) => Tracking::Process(name.clone()),
            None if is_uri(&app.exe) => Tracking::Untracked,
            None => Tracking::Job,
        }
    }

    pub fn kind(&self) -> TrackingKind {
        match self {
            Tracking::Job => TrackingKind::Job,
            Tracking::Process(_) => TrackingKind::Process,
            Tracking::Untracked => TrackingKind::Untracked,
        }
    }
}

/// Whether `exe` is a URI (`steam://…`) rather than a path.
pub fn is_uri(exe: &str) -> bool {
    exe.contains("://")
}

/// Whether `name` is usable as `wait_process`: a bare image name such as
/// `Game.exe`, which is all a process snapshot reports.
pub fn valid_process_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(['\\', '/', ':'])
        && name.len() > 4
        && name[name.len() - 4..].eq_ignore_ascii_case(".exe")
}

/// One look at the machine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Observation {
    /// Processes still in the launch's job, if there is one. With no job (a
    /// URI), `None`.
    pub job_active: Option<u32>,
    /// Whether the `wait_process` image is running, when there is one.
    pub process_present: Option<bool>,
}

/// What an observation says about the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Launched, and the process it will be judged by has not appeared yet.
    Starting,
    Running,
    Exited(ExitReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    /// Every process in the job has exited.
    JobEmpty,
    /// The watched process was seen, and has been gone past the debounce.
    Gone,
    /// The watched process never appeared within the grace period, and the
    /// launcher is no longer running.
    NeverAppeared,
}

/// Follows one launch.
#[derive(Debug)]
pub struct Tracker {
    tracking: Tracking,
    started_ms: u64,
    /// The watched process has been seen at least once.
    seen: bool,
    absent_since_ms: Option<u64>,
}

impl Tracker {
    pub fn new(tracking: Tracking, started_ms: u64) -> Tracker {
        Tracker {
            tracking,
            started_ms,
            seen: false,
            absent_since_ms: None,
        }
    }

    pub fn tracking(&self) -> &Tracking {
        &self.tracking
    }

    pub fn observe(&mut self, now_ms: u64, obs: Observation) -> Verdict {
        match &self.tracking {
            Tracking::Untracked => Verdict::Running,
            Tracking::Job => match obs.job_active {
                Some(0) => Verdict::Exited(ExitReason::JobEmpty),
                _ => Verdict::Running,
            },
            Tracking::Process(_) => {
                if obs.process_present == Some(true) {
                    self.seen = true;
                    self.absent_since_ms = None;
                    return Verdict::Running;
                }
                if !self.seen {
                    let launcher_alive = obs.job_active.is_some_and(|n| n > 0);
                    let in_grace = now_ms.saturating_sub(self.started_ms) < LAUNCH_GRACE_MS;
                    return if launcher_alive || in_grace {
                        Verdict::Starting
                    } else {
                        Verdict::Exited(ExitReason::NeverAppeared)
                    };
                }
                let since = *self.absent_since_ms.get_or_insert(now_ms);
                if now_ms.saturating_sub(since) >= EXIT_DEBOUNCE_MS {
                    Verdict::Exited(ExitReason::Gone)
                } else {
                    Verdict::Running
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(exe: &str, wait: Option<&str>) -> AppEntry {
        AppEntry {
            exe: exe.into(),
            wait_process: wait.map(Into::into),
            ..Default::default()
        }
    }

    fn job(n: u32) -> Observation {
        Observation {
            job_active: Some(n),
            process_present: None,
        }
    }

    fn proc(present: bool, launcher: Option<u32>) -> Observation {
        Observation {
            job_active: launcher,
            process_present: Some(present),
        }
    }

    #[test]
    fn tracking_follows_the_entry() {
        assert_eq!(
            Tracking::for_entry(&entry(r"C:\g\game.exe", None)),
            Tracking::Job
        );
        assert_eq!(
            Tracking::for_entry(&entry("steam://open/bigpicture", None)),
            Tracking::Untracked
        );
        assert_eq!(
            Tracking::for_entry(&entry("steam://rungameid/1", Some("Game.exe"))),
            Tracking::Process("Game.exe".into())
        );
        assert_eq!(
            Tracking::for_entry(&entry(r"C:\epic\launcher.exe", Some("Game.exe"))),
            Tracking::Process("Game.exe".into())
        );
    }

    #[test]
    fn process_names_are_bare_images() {
        assert!(valid_process_name("Game.exe"));
        assert!(valid_process_name("GAME.EXE"));
        for bad in [
            "",
            ".exe",
            "game",
            r"C:\g\game.exe",
            "g/game.exe",
            "game.exe:1",
        ] {
            assert!(!valid_process_name(bad), "{bad:?} passed");
        }
    }

    #[test]
    fn a_job_runs_while_anything_in_it_does() {
        let mut t = Tracker::new(Tracking::Job, 0);
        assert_eq!(t.observe(1_000, job(1)), Verdict::Running);
        // The launcher exited, the game it started is still in the job.
        assert_eq!(t.observe(2_000, job(1)), Verdict::Running);
        assert_eq!(
            t.observe(3_000, job(0)),
            Verdict::Exited(ExitReason::JobEmpty)
        );
    }

    #[test]
    fn an_untracked_launch_never_exits_on_its_own() {
        let mut t = Tracker::new(Tracking::Untracked, 0);
        assert_eq!(
            t.observe(10_000_000, Observation::default()),
            Verdict::Running
        );
    }

    #[test]
    fn a_watched_game_starts_runs_and_exits_after_the_debounce() {
        let mut t = Tracker::new(Tracking::Process("Game.exe".into()), 0);
        assert_eq!(t.observe(1_000, proc(false, None)), Verdict::Starting);
        assert_eq!(t.observe(20_000, proc(true, None)), Verdict::Running);
        // Gone, but not yet for long: a bootstrapper restarting the game.
        assert_eq!(t.observe(30_000, proc(false, None)), Verdict::Running);
        assert_eq!(t.observe(31_000, proc(true, None)), Verdict::Running);
        assert_eq!(t.observe(40_000, proc(false, None)), Verdict::Running);
        assert_eq!(
            t.observe(40_000 + EXIT_DEBOUNCE_MS, proc(false, None)),
            Verdict::Exited(ExitReason::Gone)
        );
    }

    #[test]
    fn a_game_that_never_appears_is_given_up_on_after_the_grace() {
        let mut t = Tracker::new(Tracking::Process("Game.exe".into()), 0);
        assert_eq!(
            t.observe(LAUNCH_GRACE_MS - 1, proc(false, None)),
            Verdict::Starting
        );
        assert_eq!(
            t.observe(LAUNCH_GRACE_MS, proc(false, None)),
            Verdict::Exited(ExitReason::NeverAppeared)
        );
    }

    #[test]
    fn a_launcher_still_running_extends_the_grace() {
        // An update or a login prompt in the launcher, well past two minutes.
        let mut t = Tracker::new(Tracking::Process("Game.exe".into()), 0);
        assert_eq!(t.observe(600_000, proc(false, Some(1))), Verdict::Starting);
        assert_eq!(t.observe(601_000, proc(true, Some(0))), Verdict::Running);
    }
}

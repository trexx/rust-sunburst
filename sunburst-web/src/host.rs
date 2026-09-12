// SPDX-License-Identifier: GPL-2.0-or-later

//! The seam between the API and the machine it runs on.
//!
//! Everything Windows-specific the web UI needs lives behind this trait:
//! launching a program, toggling autostart, restarting the server, and looking
//! at live sessions. `sunburst-server` implements it against Win32; [`Fake`]
//! implements it for tests.
//!
//! This exists to make the API testable on the Linux development machine, not
//! for abstraction's sake. Without it, none of `api.rs` could be exercised
//! without the 4070 box.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::config::AppEntry;

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("no such app: {0}")]
    UnknownApp(u32),
    #[error("no such session: {0}")]
    UnknownSession(u32),
    #[error("an app is already running: {0}")]
    AlreadyRunning(u32),
    #[error("{0}")]
    Failed(String),
    #[error("not supported on this platform")]
    Unsupported,
}

/// How the server process itself is doing.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostStatus {
    pub pid: u32,
    pub uptime_secs: u64,
    /// Whether the process is elevated.
    ///
    /// Surfaced because it decides whether `SendInput` can reach an elevated
    /// window past UIPI — the difference between input working everywhere and
    /// input mysteriously doing nothing in one game.
    pub elevated: bool,
    pub version: String,
}

/// A currently streaming client.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: u32,
    pub client_id: u32,
    pub client_name: String,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub started_at: u64,
    pub app_id: Option<u32>,
}

/// A launched app the server is tracking.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunningApp {
    pub app_id: u32,
    pub pid: u32,
    pub started_at: u64,
}

pub trait Host: Send + Sync {
    fn status(&self) -> HostStatus;

    /// Launch, running the entry's `prep` commands first.
    fn launch(&self, app: &AppEntry) -> Result<RunningApp, HostError>;

    /// Terminate the running app and undo its `prep` commands.
    fn terminate(&self) -> Result<(), HostError>;

    fn running_app(&self) -> Option<RunningApp>;

    fn autostart(&self) -> Result<bool, HostError>;
    fn set_autostart(&self, enabled: bool) -> Result<(), HostError>;

    /// Relaunch the server. The current process exits, so this does not return
    /// on success in a real implementation.
    fn restart_self(&self) -> Result<(), HostError>;

    fn sessions(&self) -> Vec<SessionSummary>;
    fn disconnect(&self, session_id: u32) -> Result<(), HostError>;

    /// A snapshot of the instrumentation ring, if a drain thread is running.
    ///
    /// Lives here because the server owns the `DrainHandle`. Reusing Phase 1
    /// rather than adding a second measurement path is deliberate: a metric that
    /// disagrees with the PR gate would be worse than no metric at all.
    fn metrics(&self) -> Option<sunburst_core::instr::Report> {
        None
    }
}

/// An in-memory `Host` for tests.
///
/// Shipped rather than hidden behind a feature because the API tests are the
/// main reason the trait exists, and a test double that is awkward to reach is
/// a test double that does not get used.
#[derive(Default)]
pub struct Fake {
    state: Mutex<FakeState>,
}

#[derive(Default)]
struct FakeState {
    running: Option<RunningApp>,
    autostart: bool,
    sessions: Vec<SessionSummary>,
    pub restarts: u32,
    next_pid: u32,
    /// Set to make every fallible call fail, for the error paths.
    fail: bool,
    metrics: Option<sunburst_core::instr::Report>,
}

impl Fake {
    pub fn new() -> Fake {
        Fake::default()
    }

    pub fn with_sessions(sessions: Vec<SessionSummary>) -> Fake {
        let fake = Fake::new();
        fake.state.lock().expect("not poisoned").sessions = sessions;
        fake
    }

    /// Make every fallible operation fail, so the API's error paths are reachable.
    pub fn set_failing(&self, fail: bool) {
        self.state.lock().expect("not poisoned").fail = fail;
    }

    pub fn set_metrics(&self, report: Option<sunburst_core::instr::Report>) {
        self.state.lock().expect("not poisoned").metrics = report;
    }

    pub fn restarts(&self) -> u32 {
        self.state.lock().expect("not poisoned").restarts
    }
}

impl Host for Fake {
    fn status(&self) -> HostStatus {
        HostStatus {
            pid: 4242,
            uptime_secs: 60,
            elevated: false,
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    fn launch(&self, app: &AppEntry) -> Result<RunningApp, HostError> {
        let mut state = self.state.lock().expect("not poisoned");
        if state.fail {
            return Err(HostError::Failed("fake failure".into()));
        }
        if let Some(running) = &state.running {
            return Err(HostError::AlreadyRunning(running.app_id));
        }
        state.next_pid += 1;
        let running = RunningApp {
            app_id: app.id,
            pid: 1000 + state.next_pid,
            started_at: 1_700_000_000,
        };
        state.running = Some(running.clone());
        Ok(running)
    }

    fn terminate(&self) -> Result<(), HostError> {
        let mut state = self.state.lock().expect("not poisoned");
        if state.fail {
            return Err(HostError::Failed("fake failure".into()));
        }
        state.running = None;
        Ok(())
    }

    fn running_app(&self) -> Option<RunningApp> {
        self.state.lock().expect("not poisoned").running.clone()
    }

    fn autostart(&self) -> Result<bool, HostError> {
        let state = self.state.lock().expect("not poisoned");
        if state.fail {
            return Err(HostError::Failed("fake failure".into()));
        }
        Ok(state.autostart)
    }

    fn set_autostart(&self, enabled: bool) -> Result<(), HostError> {
        let mut state = self.state.lock().expect("not poisoned");
        if state.fail {
            return Err(HostError::Failed("fake failure".into()));
        }
        state.autostart = enabled;
        Ok(())
    }

    fn restart_self(&self) -> Result<(), HostError> {
        let mut state = self.state.lock().expect("not poisoned");
        if state.fail {
            return Err(HostError::Failed("fake failure".into()));
        }
        state.restarts += 1;
        Ok(())
    }

    fn sessions(&self) -> Vec<SessionSummary> {
        self.state.lock().expect("not poisoned").sessions.clone()
    }

    fn metrics(&self) -> Option<sunburst_core::instr::Report> {
        self.state.lock().expect("not poisoned").metrics.clone()
    }

    fn disconnect(&self, session_id: u32) -> Result<(), HostError> {
        let mut state = self.state.lock().expect("not poisoned");
        if state.fail {
            return Err(HostError::Failed("fake failure".into()));
        }
        let before = state.sessions.len();
        state.sessions.retain(|s| s.id != session_id);
        if state.sessions.len() == before {
            return Err(HostError::UnknownSession(session_id));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: u32) -> AppEntry {
        AppEntry {
            id,
            name: "Game".into(),
            exe: "game.exe".into(),
            ..Default::default()
        }
    }

    #[test]
    fn launching_twice_is_refused() {
        let host = Fake::new();
        host.launch(&app(1)).expect("first launch");
        assert!(matches!(
            host.launch(&app(2)),
            Err(HostError::AlreadyRunning(1))
        ));
    }

    #[test]
    fn terminate_clears_the_running_app() {
        let host = Fake::new();
        host.launch(&app(1)).expect("launch");
        assert!(host.running_app().is_some());
        host.terminate().expect("terminate");
        assert!(host.running_app().is_none());
    }

    #[test]
    fn autostart_round_trips() {
        let host = Fake::new();
        assert!(!host.autostart().expect("read"));
        host.set_autostart(true).expect("set");
        assert!(host.autostart().expect("read"));
    }

    #[test]
    fn disconnecting_an_unknown_session_is_an_error() {
        let host = Fake::new();
        assert!(matches!(
            host.disconnect(9),
            Err(HostError::UnknownSession(9))
        ));
    }

    #[test]
    fn failure_mode_reaches_every_fallible_call() {
        let host = Fake::new();
        host.set_failing(true);
        assert!(host.launch(&app(1)).is_err());
        assert!(host.terminate().is_err());
        assert!(host.autostart().is_err());
        assert!(host.set_autostart(true).is_err());
        assert!(host.restart_self().is_err());
        assert!(host.disconnect(1).is_err());
    }
}

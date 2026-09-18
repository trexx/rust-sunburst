// SPDX-License-Identifier: GPL-2.0-or-later

//! The Windows implementation of [`sunburst_web::Host`].
//!
//! # No service
//!
//! Capture and `SendInput` both require the interactive session, so nothing
//! useful can live in session 0. The server runs in the logged-in session and is
//! started at logon by a scheduled task. That drops a service, an installer, and
//! a session-0 IPC surface — and it means launching an app is a plain
//! `CreateProcess` rather than `WTSGetActiveConsoleSessionId` followed by
//! `CreateProcessAsUser`.
//!
//! The cost is stated rather than hidden: **with nobody logged in there is no
//! server and no web UI.** Capture could not work in that state either, so a
//! service would not have recovered anything a user has to be present for.

use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sunburst_core::instr::{DrainHandle, Report};
use sunburst_web::config::AppEntry;
use sunburst_web::host::{Host, HostError, HostStatus, RunningApp, SessionSummary};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS, GetCurrentProcess, GetCurrentProcessId,
    OpenProcessToken,
};
use windows::core::HSTRING;

/// The scheduled-task name used for autostart.
const TASK_NAME: &str = "Sunburst";

/// Grace period before a restart actually exits.
///
/// The HTTP response has to reach the browser first, or the UI reports a dropped
/// connection instead of "restarting".
const RESTART_GRACE: Duration = Duration::from_millis(500);

pub struct WindowsHost {
    started: Instant,
    running: Mutex<Option<Launched>>,
    drain: Option<DrainHandle>,
    /// The live-session view, shared with the session manager on the endpoint
    /// thread. Reads for `GET /api/sessions`; a disconnect is requested through
    /// it and acted on by the manager.
    sessions: Arc<crate::session::Sessions>,
}

struct Launched {
    app: AppEntry,
    info: RunningApp,
    /// `None` for a URI launch: `ShellExecute` hands the request to whatever is
    /// registered — Steam, usually — and there is no child of ours to wait on.
    child: Option<std::process::Child>,
}

impl WindowsHost {
    pub fn new(drain: Option<DrainHandle>, sessions: Arc<crate::session::Sessions>) -> WindowsHost {
        WindowsHost {
            started: Instant::now(),
            running: Mutex::new(None),
            drain,
            sessions,
        }
    }

    fn current_exe() -> Result<std::path::PathBuf, HostError> {
        std::env::current_exe().map_err(|e| HostError::Failed(format!("current_exe: {e}")))
    }
}

impl Host for WindowsHost {
    fn status(&self) -> HostStatus {
        HostStatus {
            // SAFETY: takes no arguments and cannot fail.
            pid: unsafe { GetCurrentProcessId() },
            uptime_secs: self.started.elapsed().as_secs(),
            elevated: is_elevated().unwrap_or(false),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    fn launch(&self, app: &AppEntry) -> Result<RunningApp, HostError> {
        let mut running = self.running.lock().expect("not poisoned");
        if let Some(existing) = running.as_ref() {
            return Err(HostError::AlreadyRunning(existing.info.app_id));
        }

        // Prep first, and abort the launch if any of it fails: a game started
        // without its resolution or HDR change is a worse outcome than one that
        // did not start, because the failure is invisible until it is on screen.
        for step in &app.prep {
            run_shell(&step.run).map_err(|e| {
                HostError::Failed(format!("prep command `{}` failed: {e}", step.run))
            })?;
        }

        let child = if app.exe.contains("://") {
            shell_execute(&app.exe)?;
            None
        } else {
            let mut command = Command::new(&app.exe);
            command
                .args(&app.args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if let Some(dir) = &app.working_dir {
                command.current_dir(dir);
            }
            Some(
                command
                    .spawn()
                    .map_err(|e| HostError::Failed(format!("could not launch {}: {e}", app.exe)))?,
            )
        };

        let info = RunningApp {
            app_id: app.id,
            pid: child.as_ref().map_or(0, |c| c.id()),
            started_at: unix_now(),
        };
        *running = Some(Launched {
            app: app.clone(),
            info: info.clone(),
            child,
        });
        Ok(info)
    }

    fn terminate(&self) -> Result<(), HostError> {
        let mut running = self.running.lock().expect("not poisoned");
        let Some(mut launched) = running.take() else {
            return Ok(());
        };

        if let Some(child) = launched.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }

        // Undo in reverse: a prep list is a stack of changes, and undoing a
        // resolution change before the HDR toggle that depended on it leaves the
        // display in a state neither step expected.
        for step in launched.app.prep.iter().rev() {
            if let Some(undo) = &step.undo {
                // A failed undo is reported but does not stop the rest. Giving
                // up halfway would leave more of the display changed, not less.
                let _ = run_shell(undo);
            }
        }
        Ok(())
    }

    fn running_app(&self) -> Option<RunningApp> {
        let mut running = self.running.lock().expect("not poisoned");
        let launched = running.as_mut()?;

        // Reap first: an app the user closed themselves should stop being
        // reported as running, or the next launch is refused with a conflict.
        if let Some(child) = launched.child.as_mut()
            && matches!(child.try_wait(), Ok(Some(_)))
        {
            *running = None;
            return None;
        }
        running.as_ref().map(|l| l.info.clone())
    }

    fn autostart(&self) -> Result<bool, HostError> {
        let output = Command::new("schtasks")
            .args(["/Query", "/TN", TASK_NAME])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| HostError::Failed(format!("schtasks: {e}")))?;
        Ok(output.success())
    }

    fn set_autostart(&self, enabled: bool) -> Result<(), HostError> {
        // `schtasks` rather than the Task Scheduler COM API. This runs twice in
        // the life of an install, and the COM interface is a great deal of
        // unsafe for something a documented command-line tool already does.
        let status = if enabled {
            let exe = Self::current_exe()?;
            Command::new("schtasks")
                .args(["/Create", "/TN", TASK_NAME, "/TR"])
                .arg(&exe)
                // ONLOGON rather than a Run key: only a scheduled task can ask
                // for highest privileges, and without those SendInput cannot
                // reach an elevated window past UIPI.
                .args(["/SC", "ONLOGON", "/RL", "HIGHEST", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
        } else {
            Command::new("schtasks")
                .args(["/Delete", "/TN", TASK_NAME, "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
        }
        .map_err(|e| HostError::Failed(format!("schtasks: {e}")))?;

        if status.success() {
            Ok(())
        } else {
            Err(HostError::Failed(format!(
                "schtasks exited with {status}. Creating a HIGHEST task needs an \
                 elevated server; check the elevated flag in status"
            )))
        }
    }

    fn restart_self(&self) -> Result<(), HostError> {
        let exe = Self::current_exe()?;
        Command::new(exe)
            .creation_flags((DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP).0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| HostError::Failed(format!("could not relaunch: {e}")))?;

        // The replacement retries its bind for a few seconds (see
        // `sunburst_web::http::bind`), so it will be waiting for this process to
        // let go of the port rather than losing the race and dying.
        std::thread::spawn(|| {
            std::thread::sleep(RESTART_GRACE);
            std::process::exit(0);
        });
        Ok(())
    }

    fn sessions(&self) -> Vec<SessionSummary> {
        self.sessions.list()
    }

    fn disconnect(&self, session_id: u32) -> Result<(), HostError> {
        // The manager owns the pipeline; ask it to drop this one. It acts on the
        // request on its next tick.
        if self.sessions.request_disconnect(session_id) {
            Ok(())
        } else {
            Err(HostError::UnknownSession(session_id))
        }
    }

    fn metrics(&self) -> Option<Report> {
        self.drain.as_ref()?.report()
    }
}

/// Whether this process is running elevated.
fn is_elevated() -> Result<bool, HostError> {
    let mut token = HANDLE::default();
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle needing no close, and
    // `token` is a valid out pointer.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(|e| HostError::Failed(format!("OpenProcessToken: {e}")))?;

    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0u32;
    // SAFETY: `token` is live, and the buffer and its declared size match the
    // TOKEN_ELEVATION the class expects.
    let result = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some((&raw mut elevation).cast()),
            u32::try_from(size_of::<TOKEN_ELEVATION>()).expect("fits in u32"),
            &mut returned,
        )
    };
    // SAFETY: `token` came from OpenProcessToken and is closed exactly once.
    unsafe { CloseHandle(token) }.ok();

    result.map_err(|e| HostError::Failed(format!("GetTokenInformation: {e}")))?;
    Ok(elevation.TokenIsElevated != 0)
}

/// Hand a URI to whatever is registered for it.
fn shell_execute(target: &str) -> Result<(), HostError> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let target = HSTRING::from(target);
    // SAFETY: both strings outlive the call and the window handle is null,
    // which ShellExecuteW documents as "no owner window".
    let result = unsafe {
        ShellExecuteW(
            None,
            &HSTRING::from("open"),
            &target,
            None,
            None,
            SW_SHOWNORMAL,
        )
    };

    // ShellExecuteW returns a fake HINSTANCE; anything at or below 32 is an
    // error code rather than a handle. This is the documented contract, odd as
    // it looks.
    if result.0 as isize <= 32 {
        return Err(HostError::Failed(format!(
            "ShellExecute failed for {target} with code {}",
            result.0 as isize
        )));
    }
    Ok(())
}

fn run_shell(command: &str) -> Result<(), String> {
    let status = Command::new("cmd")
        .args(["/C", command])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("exited with {status}"))
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

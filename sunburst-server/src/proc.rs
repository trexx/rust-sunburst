// SPDX-License-Identifier: GPL-2.0-or-later

//! Following a launched app's processes: a job object for the tree we start,
//! and a process snapshot for the game a launcher hands off to.
//!
//! What to conclude from these is `sunburst_web::apptrack`'s business; this is
//! only the Win32 that gathers the observations, kept thin per CLAUDE.md's FFI
//! rule.

use std::io;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command};
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};
use windows::Win32::System::Threading::{
    CREATE_SUSPENDED, GetProcessTimes, OpenProcess, OpenThread, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_TERMINATE, ResumeThread, THREAD_SUSPEND_RESUME, TerminateProcess,
};

/// A job object holding everything one launch starts.
///
/// **No `KILL_ON_JOB_CLOSE`.** Closing the handle — the server restarting,
/// updating, or crashing — must not take the game down with it. The price,
/// stated: after a server restart the game runs on untracked, and its prep is
/// not undone when it exits.
///
/// **`BREAKAWAY_OK`**, because some launchers create the game with
/// `CREATE_BREAKAWAY_FROM_JOB`, and in a job that forbids it that fails with
/// access denied: the game would not start at all. A process that breaks away
/// leaves the job, which is exactly the case `wait_process` exists for.
pub struct Job(HANDLE);

// SAFETY: a job handle is a kernel object reference, usable from any thread.
unsafe impl Send for Job {}

impl Job {
    pub fn new() -> io::Result<Job> {
        // SAFETY: no name, default security; the handle is owned by `Job`.
        let handle = unsafe { CreateJobObjectW(None, None) }.map_err(io::Error::other)?;
        let job = Job(handle);
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_BREAKAWAY_OK;
        // SAFETY: `limits` is the struct this information class takes, sized
        // as passed.
        unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        }
        .map_err(io::Error::other)?;
        Ok(job)
    }

    /// Processes still in the job, or `None` if the query failed.
    pub fn active_processes(&self) -> Option<u32> {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: `info` is the struct this information class fills, sized as
        // passed; the returned length is not needed.
        unsafe {
            QueryInformationJobObject(
                Some(self.0),
                JobObjectBasicAccountingInformation,
                (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                None,
            )
        }
        .ok()
        .map(|()| info.ActiveProcesses)
    }

    /// End every process in the job.
    pub fn terminate(&self) {
        // SAFETY: a live job handle.
        let _ = unsafe { TerminateJobObject(self.0, 1) };
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: owned since `new`, closed once. Not KILL_ON_JOB_CLOSE, so the
        // processes carry on.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Start `command` inside `job`: created suspended, assigned, then resumed, so
/// nothing it starts can escape the job by racing the assignment. If the
/// assignment fails, the process is resumed anyway and `Ok((child, false))`
/// says it is not in the job.
pub fn spawn_in_job(command: &mut Command, job: &Job) -> io::Result<(Child, bool)> {
    let child = command.creation_flags(CREATE_SUSPENDED.0).spawn()?;
    let process = HANDLE(child.as_raw_handle());
    // SAFETY: both handles are live; `child` owns the process handle.
    let assigned = unsafe { AssignProcessToJobObject(job.0, process) }.is_ok();
    resume(child.id());
    Ok((child, assigned))
}

/// Resume every thread of a process created suspended (its one thread).
fn resume(pid: u32) {
    // SAFETY: a snapshot of every thread; closed below.
    let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }) else {
        return;
    };
    let mut entry = THREADENTRY32 {
        dwSize: size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    // SAFETY: `entry.dwSize` is set, as the walk requires.
    let mut more = unsafe { Thread32First(snap, &mut entry) }.is_ok();
    while more {
        if entry.th32OwnerProcessID == pid
            // SAFETY: a thread id from the snapshot; the handle is closed below.
            && let Ok(thread) = unsafe { OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID) }
        {
            // SAFETY: a live thread handle with resume access, closed once.
            unsafe {
                ResumeThread(thread);
                let _ = CloseHandle(thread);
            }
        }
        // SAFETY: as above.
        more = unsafe { Thread32Next(snap, &mut entry) }.is_ok();
    }
    // SAFETY: the snapshot handle, closed once.
    let _ = unsafe { CloseHandle(snap) };
}

/// Call `f(pid, image name)` for every running process.
fn for_each_process(mut f: impl FnMut(u32, String)) {
    // SAFETY: a snapshot of every process; closed below.
    let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return;
    };
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    // SAFETY: `entry.dwSize` is set, as the walk requires.
    let mut more = unsafe { Process32FirstW(snap, &mut entry) }.is_ok();
    while more {
        let len = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        f(
            entry.th32ProcessID,
            String::from_utf16_lossy(&entry.szExeFile[..len]),
        );
        // SAFETY: as above.
        more = unsafe { Process32NextW(snap, &mut entry) }.is_ok();
    }
    // SAFETY: the snapshot handle, closed once.
    let _ = unsafe { CloseHandle(snap) };
}

/// Each running process whose image name is `name` (case-insensitive), by pid.
fn processes_named(name: &str) -> Vec<u32> {
    let mut pids = Vec::new();
    for_each_process(|pid, image| {
        if image.eq_ignore_ascii_case(name) {
            pids.push(pid);
        }
    });
    pids
}

/// Every running process's image name, by pid.
pub fn process_names() -> std::collections::HashMap<u32, String> {
    let mut names = std::collections::HashMap::new();
    for_each_process(|pid, image| {
        names.insert(pid, image);
    });
    names
}

/// Whether a process with image name `name` is running.
pub fn process_running(name: &str) -> bool {
    !processes_named(name).is_empty()
}

/// Seconds since the Unix epoch for a `FILETIME` (100 ns ticks since 1601).
fn filetime_unix_secs(t: FILETIME) -> u64 {
    const EPOCH_DIFF_SECS: u64 = 11_644_473_600;
    let ticks = u64::from(t.dwHighDateTime) << 32 | u64::from(t.dwLowDateTime);
    (ticks / 10_000_000).saturating_sub(EPOCH_DIFF_SECS)
}

/// End each process named `name` that was created at or after `since`, so a
/// same-named process that predates the launch is left alone.
pub fn kill_named_since(name: &str, since: SystemTime) {
    let since = since.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    for pid in processes_named(name) {
        // SAFETY: a pid from the snapshot; the handle is closed below.
        let Ok(process) = (unsafe {
            OpenProcess(
                PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                false,
                pid,
            )
        }) else {
            continue;
        };
        let (mut created, mut exited, mut kernel, mut user) = Default::default();
        // SAFETY: a live process handle with query access; out-params are
        // plain FILETIMEs.
        let times =
            unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) };
        if times.is_ok() && filetime_unix_secs(created) >= since {
            // SAFETY: a live process handle with terminate access.
            let _ = unsafe { TerminateProcess(process, 1) };
        }
        // SAFETY: closed once.
        let _ = unsafe { CloseHandle(process) };
    }
}

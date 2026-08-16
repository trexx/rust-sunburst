// SPDX-License-Identifier: GPL-2.0-or-later

//! `SendInput`, on its own thread.
//!
//! Thin transcription of what [`crate::keymap`] decided, per CLAUDE.md's FFI
//! rule. The decisions are tested there; this only performs them.
//!
//! # Why a dedicated thread
//!
//! Two reasons, and the second is the binding one. `SendInput` can block under
//! contention and must not stall the endpoint's receive loop. And **desktop
//! attachment is per-thread**, so whichever thread calls `SendInput` has to be
//! the thread that attached — which means it cannot be a pool.
//!
//! # Desktop re-attach
//!
//! `SendInput` targets the calling thread's desktop. UAC and the lock screen
//! switch the desktop *inside* the session, so a process that never went
//! anywhere still finds itself attached to the wrong one and its input silently
//! goes nowhere.
//!
//! There is no notification for a desktop switch — `WTSRegisterSessionNotification`
//! reports session changes, which this is not — so the thread polls.
//! `OpenInputDesktop` hands back a fresh handle every call, so the comparison is
//! by **name**: two handles to the same desktop are different values.
//!
//! On the secure desktop `SetThreadDesktop` fails without the privilege to
//! attach, and input goes nowhere until the user dismisses the prompt. That is
//! expected rather than an error, and matches capture, which cannot see it
//! either.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::time::Duration;

use sunburst_core::proto::{InputEvent, MouseMotion};
use sunburst_net::InputSink;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, GetUserObjectInformationW, HDESK, OpenInputDesktop,
    SetThreadDesktop, UOI_NAME,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    MAP_VIRTUAL_KEY_TYPE, MOUSE_EVENT_FLAGS, MOUSEINPUT, MapVirtualKeyW, SendInput,
};

use crate::keymap::{self, KeyAction, Modifiers, MouseAction};

/// `MAPVK_VK_TO_VSC_EX`, which returns the `0xE0`/`0xE1` prefix that
/// `MAPVK_VK_TO_VSC` throws away.
const MAPVK_VK_TO_VSC_EX: MAP_VIRTUAL_KEY_TYPE = MAP_VIRTUAL_KEY_TYPE(4);

/// Queue depth. At the ~1000 events/sec input tops out at, this is a quarter of
/// a second of backlog — far more than `SendInput` should ever need.
const QUEUE_DEPTH: usize = 1024;

/// How often to check whether the desktop moved under us.
const DESKTOP_POLL: Duration = Duration::from_millis(200);

/// Counters worth having when input "does nothing".
#[derive(Default)]
pub struct Stats {
    /// Events discarded because the queue was full.
    pub dropped: AtomicU64,
    /// `SendInput` returning fewer events than it was given.
    pub refused: AtomicU64,
    /// Desktop switches successfully followed.
    pub reattached: AtomicU64,
    /// Switches that could not be followed — the secure desktop, normally.
    pub attach_failed: AtomicU64,
}

pub struct Injector {
    tx: SyncSender<InputEvent>,
    stats: Arc<Stats>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Injector {
    pub fn start() -> std::io::Result<Injector> {
        let (tx, rx) = sync_channel(QUEUE_DEPTH);
        let stats = Arc::new(Stats::default());
        let thread_stats = Arc::clone(&stats);

        let thread = std::thread::Builder::new()
            .name("sunburst-input".into())
            .spawn(move || run(rx, &thread_stats))?;

        Ok(Injector {
            tx,
            stats,
            thread: Some(thread),
        })
    }

    pub fn stats(&self) -> &Arc<Stats> {
        &self.stats
    }
}

impl InputSink for Injector {
    fn inject(&mut self, _client: u32, event: InputEvent) {
        // Never block: the caller is the endpoint's receive loop, and stalling
        // it would back up every client rather than just this one.
        match self.tx.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        // Dropping the sender ends the thread's recv loop.
        if let Some(thread) = self.thread.take() {
            let (dead, _) = sync_channel(1);
            drop(std::mem::replace(&mut self.tx, dead));
            let _ = thread.join();
        }
    }
}

fn run(rx: Receiver<InputEvent>, stats: &Stats) {
    let mut desktop = Desktop::default();
    let mut modifiers = Modifiers::new();

    loop {
        match rx.recv_timeout(DESKTOP_POLL) {
            Ok(event) => {
                desktop.ensure(stats);
                apply(event, &mut modifiers, stats);
            }
            // Idle. Still worth checking, so the first event after a UAC prompt
            // is not the one that gets lost.
            Err(RecvTimeoutError::Timeout) => desktop.ensure(stats),
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    // Whatever the last client left held, so a disconnect mid-chord does not
    // leave Ctrl down on the desktop.
    for action in modifiers.release_all() {
        send_key(action, stats);
    }
}

fn apply(event: InputEvent, modifiers: &mut Modifiers, stats: &Stats) {
    match event {
        InputEvent::KeyDown { vk, modifiers: m } => {
            for action in modifiers.resolve(vk, true, m) {
                send_key(action, stats);
            }
        }
        InputEvent::KeyUp { vk, modifiers: m } => {
            for action in modifiers.resolve(vk, false, m) {
                send_key(action, stats);
            }
        }
        InputEvent::MouseMove(MouseMotion::Relative { dx, dy }) => {
            send_mouse(keymap::relative_action(dx, dy), stats);
        }
        InputEvent::MouseMove(MouseMotion::Absolute { x, y }) => {
            send_mouse(keymap::absolute_action(x, y), stats);
        }
        InputEvent::MouseButton { button, down } => {
            send_mouse(keymap::button_action(button, down), stats);
        }
        InputEvent::MouseWheel { delta, horizontal } => {
            send_mouse(keymap::wheel_action(delta, horizontal), stats);
        }
        // Gamepad is ViGEm's, and that is its own chunk. Dropping it here is
        // honest: there is nothing to inject it into yet.
        InputEvent::Gamepad(_) => {}
    }
}

fn send_key(action: KeyAction, stats: &Stats) {
    // SAFETY: MapVirtualKeyW takes a virtual key and a mapping type and returns
    // a scancode; it touches nothing of ours and cannot fail destructively.
    let scancode = unsafe { MapVirtualKeyW(u32::from(action.vk), MAPVK_VK_TO_VSC_EX) };

    let mut flags = keymap::key_flags::SCANCODE;
    if keymap::is_extended(scancode) {
        flags |= keymap::key_flags::EXTENDEDKEY;
    }
    if !action.down {
        flags |= keymap::key_flags::KEYUP;
    }

    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                // Deliberately zero. Sending the virtual key instead of the
                // scancode works on the desktop and does nothing in a game
                // reading raw input, which is the most expensive way to be
                // wrong here because it looks like it works.
                wVk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(0),
                wScan: keymap::scancode_byte(scancode),
                dwFlags: KEYBD_EVENT_FLAGS(flags),
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    send(&[input], stats);
}

fn send_mouse(action: MouseAction, stats: &Stats) {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: action.dx,
                dy: action.dy,
                mouseData: action.data as u32,
                dwFlags: MOUSE_EVENT_FLAGS(action.flags),
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    send(&[input], stats);
}

fn send(inputs: &[INPUT], stats: &Stats) {
    let size = i32::try_from(size_of::<INPUT>()).expect("INPUT fits in i32");
    // SAFETY: `inputs` is a valid slice of correctly initialised INPUT records,
    // and `size` is the size of the struct as SendInput requires.
    let sent = unsafe { SendInput(inputs, size) };
    if sent as usize != inputs.len() {
        // The usual cause is UIPI: an elevated window has focus and this process
        // is not elevated. Counted rather than logged, because this is called
        // per event.
        stats.refused.fetch_add(1, Ordering::Relaxed);
    }
}

/// The desktop this thread is attached to, followed as it changes.
#[derive(Default)]
struct Desktop {
    attached: Option<HDESK>,
    name: String,
}

impl Desktop {
    fn ensure(&mut self, stats: &Stats) {
        // SAFETY: no input flags, no inheritance, and the access requested is
        // what SetThreadDesktop needs. Returns a fresh handle each call.
        let Ok(input_desktop) = (unsafe {
            OpenInputDesktop(
                Default::default(),
                false,
                // DESKTOP_SWITCHDESKTOP | GENERIC_ALL, which is what attaching
                // and injecting between them requires.
                DESKTOP_ACCESS_FLAGS(0x0100 | 0x1000_0000),
            )
        }) else {
            stats.attach_failed.fetch_add(1, Ordering::Relaxed);
            return;
        };

        let name = desktop_name(input_desktop);
        if !name.is_empty() && name == self.name {
            // Same desktop, different handle. Close the duplicate rather than
            // leaking one every poll.
            // SAFETY: this handle is ours, is not the attached one, and is
            // closed exactly once.
            unsafe { CloseDesktop(input_desktop) }.ok();
            return;
        }

        // SAFETY: `input_desktop` is a live handle with SWITCHDESKTOP access.
        match unsafe { SetThreadDesktop(input_desktop) } {
            Ok(()) => {
                let previous = self.attached.replace(input_desktop);
                self.name = name;
                stats.reattached.fetch_add(1, Ordering::Relaxed);
                if let Some(previous) = previous {
                    // Only now: closing a desktop still set on this thread is
                    // not allowed.
                    // SAFETY: no longer attached, ours, closed once.
                    unsafe { CloseDesktop(previous) }.ok();
                }
            }
            Err(_) => {
                // The secure desktop, normally. Input goes nowhere until the
                // prompt is dismissed, which is expected rather than an error.
                stats.attach_failed.fetch_add(1, Ordering::Relaxed);
                // SAFETY: never attached, so closing it is safe and required.
                unsafe { CloseDesktop(input_desktop) }.ok();
            }
        }
    }
}

fn desktop_name(desktop: HDESK) -> String {
    let mut buffer = [0u16; 256];
    let mut needed = 0u32;
    // SAFETY: `buffer` is a valid writable buffer of the declared byte length,
    // and `needed` is a valid out pointer.
    let ok = unsafe {
        GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(buffer.as_mut_ptr().cast()),
            (size_of_val(&buffer)) as u32,
            Some(&mut needed),
        )
    };
    if ok.is_err() {
        return String::new();
    }
    let len = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}

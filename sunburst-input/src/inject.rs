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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sunburst_core::proto::input::MAX_PADS;
use sunburst_core::proto::{GamepadState, InputEvent, MouseMotion, PadOutput, TriggerEffect};
use sunburst_net::{InputSettings, InputSink, Outbound};

use crate::pad::codec::DecodedValue;
use crate::pad::outpolicy::RepeatPolicy;
use crate::pad::session::PadSession;
use crate::pad::{device, gip, install, registry, shmem};

/// What crosses the channel to the injector thread. The pad sections live on that
/// one thread, so pad lifecycle rides the same channel as input.
enum Msg {
    Input(InputEvent),
    Config(InputSettings),
    PadConnected {
        client: u32,
        pad_index: u8,
        pad_type: u8,
    },
    PadDisconnected {
        pad_index: u8,
    },
}

/// How often the thread wakes to drain pad output and repeat rumble when idle.
const OUTPUT_POLL: Duration = Duration::from_millis(8);

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
    tx: SyncSender<Msg>,
    stats: Arc<Stats>,
    /// Output effects the thread has produced, drained by the endpoint each tick.
    outbound: Arc<Mutex<Vec<Outbound>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Injector {
    /// Start the injector thread. `driver_inf`, when set, points at the vendored
    /// `hidmaestro.inf`; the thread stages that driver and creates a real device
    /// node per connected pad so games enumerate it. `None` keeps the pre-driver
    /// behaviour — sections are mapped but no node is created (useful before the
    /// driver artifact is provisioned, and on a box without it).
    pub fn start(driver_inf: Option<PathBuf>) -> std::io::Result<Injector> {
        let (tx, rx) = sync_channel(QUEUE_DEPTH);
        let stats = Arc::new(Stats::default());
        let outbound = Arc::new(Mutex::new(Vec::new()));
        let thread_stats = Arc::clone(&stats);
        let thread_outbound = Arc::clone(&outbound);

        let thread = std::thread::Builder::new()
            .name("sunburst-input".into())
            .spawn(move || run(rx, &thread_stats, &thread_outbound, driver_inf))?;

        Ok(Injector {
            tx,
            stats,
            outbound,
            thread: Some(thread),
        })
    }

    pub fn stats(&self) -> &Arc<Stats> {
        &self.stats
    }

    fn send(&self, msg: Msg) {
        // Never block: the caller is the endpoint's receive loop, and stalling
        // it would back up every client rather than just this one.
        match self.tx.try_send(msg) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

impl InputSink for Injector {
    fn inject(&mut self, _client: u32, event: InputEvent) {
        self.send(Msg::Input(event));
    }

    fn configure(&mut self, settings: InputSettings) {
        self.send(Msg::Config(settings));
    }

    fn pad_connected(&mut self, client: u32, pad_index: u8, pad_type: u8, _capabilities: u16) {
        self.send(Msg::PadConnected {
            client,
            pad_index,
            pad_type,
        });
    }

    fn pad_disconnected(&mut self, _client: u32, pad_index: u8) {
        self.send(Msg::PadDisconnected { pad_index });
    }

    fn drain_outbound(&mut self) -> Vec<Outbound> {
        match self.outbound.lock() {
            Ok(mut q) => std::mem::take(&mut *q),
            Err(_) => Vec::new(),
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

/// One plugged virtual pad: its encoding session and the driver sections it feeds.
struct PadState {
    client: u32,
    session: PadSession,
    input: shmem::Section,
    output: shmem::Section,
    doorbell: shmem::Doorbell,
    out_seen: u32,
    policy: RepeatPolicy,
    /// The last effect sent, re-sent on repeat so a dropped packet self-heals.
    last_effect: Option<Outbound>,
    /// The virtual device node, when a driver was available to create one. Held
    /// only for its `Drop`: clearing the slot on disconnect (or reusing it)
    /// removes the pad from the OS. Never read, hence the allow.
    #[allow(dead_code)]
    node: Option<device::PadNode>,
}

fn run(
    rx: Receiver<Msg>,
    stats: &Stats,
    outbound: &Arc<Mutex<Vec<Outbound>>>,
    driver_inf: Option<PathBuf>,
) {
    // Stage the driver once. If it is missing or the store install fails (no
    // elevation, unsigned on a non-test-signed box), pads still map their
    // sections — they just will not be enumerated. Report, don't abort.
    if let Some(inf) = driver_inf.as_deref() {
        if let Err(e) = install::ensure_installed(inf) {
            eprintln!("sunburst-input: driver install failed, pads will not enumerate: {e}");
        }
        // The XUSB companion (Xbox 360) is bound by hidmaestro_xusb.inf; install
        // it too when it sits beside the main INF. Best-effort — HID pads do not
        // need it.
        if let Some(xusb) = inf.parent().map(|d| d.join("hidmaestro_xusb.inf"))
            && xusb.exists()
            && let Err(e) = install::ensure_installed(&xusb)
        {
            eprintln!(
                "sunburst-input: XUSB driver install failed, Xbox 360 pads may not enumerate: {e}"
            );
        }
    }
    let driver_inf = driver_inf.as_deref();

    let mut desktop = Desktop::default();
    let mut modifiers = Modifiers::new();
    let mut pads: [Option<PadState>; MAX_PADS as usize] = std::array::from_fn(|_| None);
    let mut settings = InputSettings::default();
    let started = Instant::now();
    let mut last_ensure = started - DESKTOP_POLL;

    loop {
        match rx.recv_timeout(OUTPUT_POLL) {
            Ok(Msg::Config(s)) => settings = s,
            // Gamepad goes to shared memory, not the desktop.
            Ok(Msg::Input(InputEvent::Gamepad(state))) => {
                submit_gamepad(&mut pads, &state, settings.gamepad_deadzone)
            }
            Ok(Msg::Input(event)) => {
                desktop.ensure(stats);
                last_ensure = Instant::now();
                apply(event, &mut modifiers, stats, settings.mouse_sensitivity);
            }
            Ok(Msg::PadConnected {
                client,
                pad_index,
                pad_type,
            }) => connect_pad(&mut pads, client, pad_index, pad_type, driver_inf),
            Ok(Msg::PadDisconnected { pad_index }) => {
                if let Some(slot) = pads.get_mut(pad_index as usize) {
                    *slot = None;
                }
            }
            // Idle. Ensure the desktop, throttled — the fast poll is for output,
            // not for hammering OpenInputDesktop.
            Err(RecvTimeoutError::Timeout) => {
                if last_ensure.elapsed() >= DESKTOP_POLL {
                    desktop.ensure(stats);
                    last_ensure = Instant::now();
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }

        // Every wake: carry the pads' output effects (rumble / triggers / LED) back.
        let now_ms = started.elapsed().as_millis() as u32;
        drain_pads(&mut pads, outbound, now_ms);
    }

    // Whatever the last client left held, so a disconnect mid-chord does not
    // leave Ctrl down on the desktop.
    for action in modifiers.release_all() {
        send_key(action, stats);
    }
}

/// Plug a virtual pad: build its session and create the driver's shared sections.
/// Map the pad's shared sections, build its encoding session, and — when a driver
/// is available — create the virtual device node so a game enumerates it. The
/// sections are created *before* the node so the driver finds them when it binds.
fn connect_pad(
    pads: &mut [Option<PadState>],
    client: u32,
    pad_index: u8,
    pad_type: u8,
    driver_inf: Option<&Path>,
) {
    let Some(slot) = pads.get_mut(pad_index as usize) else {
        return;
    };
    let Some(profile) = registry::profile_for(pad_type) else {
        return;
    };
    let Ok(session) = PadSession::new(&profile) else {
        return;
    };
    let (Ok(input), Ok(output), Ok(doorbell)) = (
        shmem::Section::create(&shmem::input_name(pad_index), shmem::INPUT_SIZE),
        shmem::Section::create(&shmem::output_name(pad_index), shmem::OUTPUT_SIZE),
        shmem::Doorbell::create(pad_index),
    ) else {
        return;
    };

    // Create the device node last, and only if a driver is provisioned. A
    // failure here (unsupported Xbox path, not elevated, driver missing) leaves
    // the pad's data path intact but unenumerated — reported, not fatal.
    let node = driver_inf.and_then(|inf| {
        let spec = device::PadNodeSpec::from_profile(&profile, pad_index)?;
        match device::create(&spec, inf) {
            Ok(node) => Some(node),
            Err(e) => {
                eprintln!("sunburst-input: pad {pad_index} node not created: {e}");
                None
            }
        }
    });

    *slot = Some(PadState {
        client,
        session,
        input,
        output,
        doorbell,
        out_seen: 0,
        policy: RepeatPolicy::new(),
        last_effect: None,
        node,
    });
}

/// Encode a gamepad frame and submit it to the pad's driver section.
fn submit_gamepad(pads: &mut [Option<PadState>], state: &GamepadState, deadzone: f32) {
    let Some(Some(pad)) = pads.get_mut(state.pad_index as usize) else {
        return;
    };
    // Apply the extra radial deadzone to both sticks before emulating the pad.
    let adjusted;
    let state = if deadzone > 0.0 {
        let mut s = *state;
        (s.lx, s.ly) = keymap::apply_deadzone(s.lx, s.ly, deadzone);
        (s.rx, s.ry) = keymap::apply_deadzone(s.rx, s.ry, deadzone);
        adjusted = s;
        &adjusted
    } else {
        state
    };
    // GIP first (its own borrow ends), copied to the stack to avoid an alloc.
    let gip = pad.session.gip(state).map(|g| {
        let mut b = [0u8; gip::GIP_LEN];
        b.copy_from_slice(g);
        b
    });
    let extended = pad.session.is_extended();
    let report = pad.session.submit(state);
    let (main, ext): (&[u8], Option<&[u8]>) = if extended {
        (&[], Some(report))
    } else {
        (report, None)
    };
    shmem::submit(
        &pad.input,
        &pad.doorbell,
        main,
        gip.as_ref().map(|b| &b[..]),
        ext,
    );
}

/// Drain every pad's output ring, turn the newest report into an outbound effect,
/// and repeat a non-neutral effect on its cadence.
fn drain_pads(pads: &mut [Option<PadState>], outbound: &Arc<Mutex<Vec<Outbound>>>, now_ms: u32) {
    for (index, slot) in pads.iter_mut().enumerate() {
        let Some(pad) = slot else { continue };
        let (reports, highest) = shmem::drain_output(&pad.output, pad.out_seen);
        pad.out_seen = highest;

        // Latest-wins: only the newest report matters.
        if let Some(last) = reports.last()
            && let Some((decoded, _crc)) = pad.session.decode_output(&last.data)
        {
            let active = effect_is_active(&decoded);
            let seq = pad.policy.on_send(active, now_ms);
            let out = Outbound::PadOutput {
                client: pad.client,
                output: build_pad_output(index as u8, seq, &decoded),
            };
            pad.last_effect = Some(out.clone());
            push(outbound, out);
        }

        // Repeat a still-active effect so a dropped packet self-heals.
        if pad.policy.repeat_due(now_ms).is_some()
            && let Some(effect) = pad.last_effect.clone()
        {
            push(outbound, effect);
        }
    }
}

fn push(outbound: &Arc<Mutex<Vec<Outbound>>>, out: Outbound) {
    if let Ok(mut q) = outbound.lock() {
        q.push(out);
    }
}

/// Whether a decoded output report commands anything (so it needs repeating).
fn effect_is_active(decoded: &std::collections::BTreeMap<String, DecodedValue>) -> bool {
    let byte = |k: &str| matches!(decoded.get(k), Some(DecodedValue::Byte(b)) if *b != 0);
    let blob = |k: &str| matches!(decoded.get(k), Some(DecodedValue::Bytes(b)) if b.iter().any(|x| *x != 0));
    byte("leftMotor")
        || byte("rightMotor")
        || blob("lightbar")
        || blob("leftTriggerEffect")
        || blob("rightTriggerEffect")
}

/// Build a [`PadOutput`] from decoded output-report fields. Field names follow the
/// Sony profiles; a family that names them differently contributes nothing here
/// until its client support lands (box-verified).
fn build_pad_output(
    pad_index: u8,
    seq: u8,
    decoded: &std::collections::BTreeMap<String, DecodedValue>,
) -> PadOutput {
    let byte = |k: &str| match decoded.get(k) {
        Some(DecodedValue::Byte(b)) => *b,
        _ => 0,
    };
    // Motor bytes (0..255) widened to the wire's 16-bit range.
    let widen = |b: u8| (b as u16) * 257;
    let effect = |k: &str| match decoded.get(k) {
        Some(DecodedValue::Bytes(b)) => TriggerEffect::from_slice(b),
        _ => TriggerEffect::default(),
    };
    let led = match decoded.get("lightbar") {
        Some(DecodedValue::Bytes(b)) if b.len() >= 3 => [b[0], b[1], b[2]],
        _ => [0, 0, 0],
    };
    let flags = if byte("muteLed") != 0 {
        sunburst_core::proto::padoutput::flags::MIC_MUTED
    } else {
        0
    };
    PadOutput {
        pad_index,
        seq,
        motor_low: widen(byte("leftMotor")),
        motor_high: widen(byte("rightMotor")),
        led,
        player_led: byte("playerIndicator"),
        flags,
        left_trigger: effect("leftTriggerEffect"),
        right_trigger: effect("rightTriggerEffect"),
    }
}

fn apply(event: InputEvent, modifiers: &mut Modifiers, stats: &Stats, mouse_sensitivity: f32) {
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
            send_mouse(
                keymap::relative_action_scaled(dx, dy, mouse_sensitivity),
                stats,
            );
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
        // Gamepad is handled before `apply` (it goes to shared memory, not the
        // desktop), so this arm is unreachable — present only for exhaustiveness.
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

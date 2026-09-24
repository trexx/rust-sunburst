// SPDX-License-Identifier: GPL-2.0-or-later

//! The decisions injection makes, separated from the act of injecting.
//!
//! Everything here is a pure function, so the parts that are actually easy to
//! get wrong — extended-key flags, modifier drift, which `MOUSEEVENTF` bit an
//! X button needs — carry real tests on a machine with no `SendInput` at all.
//! `inject.rs` is then a thin transcription of what this decided.
//!
//! Same split moonlight-trexx uses to get `KeyMapper` and `StickCalibration` out
//! of code that needs a device.

use sunburst_core::proto::MouseButton;

/// `KEYEVENTF_*`, transcribed from `winuser.h`.
pub mod key_flags {
    pub const EXTENDEDKEY: u32 = 0x0001;
    pub const KEYUP: u32 = 0x0002;
    pub const SCANCODE: u32 = 0x0008;
}

/// `MOUSEEVENTF_*`, transcribed from `winuser.h`.
pub mod mouse_flags {
    pub const MOVE: u32 = 0x0001;
    pub const LEFTDOWN: u32 = 0x0002;
    pub const LEFTUP: u32 = 0x0004;
    pub const RIGHTDOWN: u32 = 0x0008;
    pub const RIGHTUP: u32 = 0x0010;
    pub const MIDDLEDOWN: u32 = 0x0020;
    pub const MIDDLEUP: u32 = 0x0040;
    pub const XDOWN: u32 = 0x0080;
    pub const XUP: u32 = 0x0100;
    pub const WHEEL: u32 = 0x0800;
    pub const HWHEEL: u32 = 0x1000;
    pub const VIRTUALDESK: u32 = 0x4000;
    pub const ABSOLUTE: u32 = 0x8000;
}

/// `XBUTTON1` / `XBUTTON2`, which travel in `mouseData` rather than the flags.
pub mod xbutton {
    pub const XBUTTON1: i32 = 0x0001;
    pub const XBUTTON2: i32 = 0x0002;
}

/// Modifier bits in PROTOCOL.md's `modifiers` byte.
///
/// The same assignment Limelight uses, so the value means the same thing to
/// anyone coming from the Moonlight side.
pub mod modifiers {
    pub const SHIFT: u8 = 0x01;
    pub const CTRL: u8 = 0x02;
    pub const ALT: u8 = 0x04;
    pub const META: u8 = 0x08;
    pub const ALL: u8 = SHIFT | CTRL | ALT | META;
}

/// Virtual-key codes this module needs to reason about.
pub mod vk {
    pub const SHIFT: u16 = 0x10;
    pub const CONTROL: u16 = 0x11;
    pub const MENU: u16 = 0x12;
    pub const LWIN: u16 = 0x5B;
    pub const RWIN: u16 = 0x5C;
    pub const LSHIFT: u16 = 0xA0;
    pub const RSHIFT: u16 = 0xA1;
    pub const LCONTROL: u16 = 0xA2;
    pub const RCONTROL: u16 = 0xA3;
    pub const LMENU: u16 = 0xA4;
    pub const RMENU: u16 = 0xA5;
}

/// Whether a scancode from `MapVirtualKeyW(vk, MAPVK_VK_TO_VSC_EX)` needs
/// `KEYEVENTF_EXTENDEDKEY`.
///
/// **Derived, not looked up.** `MAPVK_VK_TO_VSC_EX` already answers this: it
/// returns the `0xE0` or `0xE1` prefix in the high byte for exactly the keys
/// that need the flag. A hardcoded list of virtual keys would be a second source
/// of truth that drifts against the keyboard layout — see [`EXPECTED_EXTENDED`]
/// for what that list is for instead.
pub fn is_extended(scancode: u32) -> bool {
    matches!((scancode >> 8) & 0xFF, 0xE0 | 0xE1)
}

/// The byte `SendInput` wants in `wScan`.
pub fn scancode_byte(scancode: u32) -> u16 {
    (scancode & 0xFF) as u16
}

/// Virtual keys expected to come back extended.
///
/// **Documentation and a hardware-test checklist, not the mechanism.**
/// [`is_extended`] derives the answer; this records what the answer should be,
/// so a box where it differs is a finding rather than a mystery. Without the
/// flag every one of these arrives as its numpad twin — Home becomes 7, Up
/// becomes 8, Delete becomes full stop.
pub const EXPECTED_EXTENDED: &[(u16, &str)] = &[
    (0x21, "PageUp"),
    (0x22, "PageDown"),
    (0x23, "End"),
    (0x24, "Home"),
    (0x25, "Left"),
    (0x26, "Up"),
    (0x27, "Right"),
    (0x28, "Down"),
    (0x2C, "PrintScreen"),
    (0x2D, "Insert"),
    (0x2E, "Delete"),
    (0x5B, "LeftWindows"),
    (0x5C, "RightWindows"),
    (0x5D, "Applications"),
    (0x6F, "NumpadDivide"),
    (0x90, "NumLock"),
    (0xA3, "RightControl"),
    (0xA5, "RightAlt"),
];

/// Which modifier bit a virtual key is, if it is one.
pub fn modifier_of(key: u16) -> Option<u8> {
    Some(match key {
        vk::SHIFT | vk::LSHIFT | vk::RSHIFT => modifiers::SHIFT,
        vk::CONTROL | vk::LCONTROL | vk::RCONTROL => modifiers::CTRL,
        vk::MENU | vk::LMENU | vk::RMENU => modifiers::ALT,
        vk::LWIN | vk::RWIN => modifiers::META,
        _ => return None,
    })
}

/// The key to synthesise for a modifier bit. Left-hand variants, because they
/// are unambiguous where `VK_SHIFT` is not.
fn key_for(modifier: u8) -> u16 {
    match modifier {
        modifiers::SHIFT => vk::LSHIFT,
        modifiers::CTRL => vk::LCONTROL,
        modifiers::ALT => vk::LMENU,
        _ => vk::LWIN,
    }
}

/// A key press or release for the injector to perform.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KeyAction {
    pub vk: u16,
    pub down: bool,
}

/// Believed modifier state, reconciled against what the client asserts.
///
/// # Why an assertion rather than a command
///
/// PROTOCOL.md sends a `modifiers` byte beside each key event, and the client
/// *also* sends Ctrl and Shift as ordinary key events. Applying both would press
/// every modifier twice.
///
/// So the byte is treated as a statement about what should be held down, and
/// only the difference is synthesised. That is what stops a modifier sticking
/// after a client drops mid-chord — the failure moonlight-trexx's `sendKeys`
/// accumulation notes exist for, and why Ctrl+Shift+Esc is on its test list.
///
/// # Modifier keys reconcile against nothing
///
/// When the event is itself a modifier, the byte is ambiguous: a client may set
/// the bit for the key it is pressing or may not, and both conventions exist.
/// Reconciliation is skipped for those and the event is simply applied, which is
/// correct under either.
#[derive(Default, Debug)]
pub struct Modifiers {
    down: u8,
}

impl Modifiers {
    pub fn new() -> Modifiers {
        Modifiers::default()
    }

    pub fn held(&self) -> u8 {
        self.down
    }

    /// Everything to inject for one key event, in order.
    pub fn resolve(&mut self, key: u16, down: bool, asserted: u8) -> Vec<KeyAction> {
        let mut actions = Vec::new();

        if let Some(bit) = modifier_of(key) {
            // The event is a modifier. Apply it and believe it.
            if down {
                self.down |= bit;
            } else {
                self.down &= !bit;
            }
            actions.push(KeyAction { vk: key, down });
            return actions;
        }

        // Bring believed state in line before the key lands, so a chord that
        // lost its Ctrl still arrives as a chord.
        let asserted = asserted & modifiers::ALL;
        for bit in [
            modifiers::SHIFT,
            modifiers::CTRL,
            modifiers::ALT,
            modifiers::META,
        ] {
            let want = asserted & bit != 0;
            let have = self.down & bit != 0;
            if want != have {
                actions.push(KeyAction {
                    vk: key_for(bit),
                    down: want,
                });
            }
        }
        self.down = asserted;

        actions.push(KeyAction { vk: key, down });
        actions
    }

    /// Release everything still held. For a client that disconnects mid-chord.
    pub fn release_all(&mut self) -> Vec<KeyAction> {
        let mut actions = Vec::new();
        for bit in [
            modifiers::SHIFT,
            modifiers::CTRL,
            modifiers::ALT,
            modifiers::META,
        ] {
            if self.down & bit != 0 {
                actions.push(KeyAction {
                    vk: key_for(bit),
                    down: false,
                });
            }
        }
        self.down = 0;
        actions
    }
}

/// A mouse event reduced to what `SendInput` takes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MouseAction {
    pub flags: u32,
    /// `mouseData`: the X button, or the wheel delta.
    pub data: i32,
    pub dx: i32,
    pub dy: i32,
}

/// Which flag and `mouseData` a button press or release needs.
///
/// X buttons are the trap: they share one pair of flags and are told apart by
/// `mouseData`, so a mapping that only sets flags silently turns X2 into X1.
pub fn button_action(button: MouseButton, down: bool) -> MouseAction {
    let (flags, data) = match (button, down) {
        (MouseButton::Left, true) => (mouse_flags::LEFTDOWN, 0),
        (MouseButton::Left, false) => (mouse_flags::LEFTUP, 0),
        (MouseButton::Right, true) => (mouse_flags::RIGHTDOWN, 0),
        (MouseButton::Right, false) => (mouse_flags::RIGHTUP, 0),
        (MouseButton::Middle, true) => (mouse_flags::MIDDLEDOWN, 0),
        (MouseButton::Middle, false) => (mouse_flags::MIDDLEUP, 0),
        (MouseButton::X1, true) => (mouse_flags::XDOWN, xbutton::XBUTTON1),
        (MouseButton::X1, false) => (mouse_flags::XUP, xbutton::XBUTTON1),
        (MouseButton::X2, true) => (mouse_flags::XDOWN, xbutton::XBUTTON2),
        (MouseButton::X2, false) => (mouse_flags::XUP, xbutton::XBUTTON2),
    };
    MouseAction {
        flags,
        data,
        dx: 0,
        dy: 0,
    }
}

pub fn wheel_action(delta: i16, horizontal: bool) -> MouseAction {
    MouseAction {
        flags: if horizontal {
            mouse_flags::HWHEEL
        } else {
            mouse_flags::WHEEL
        },
        data: i32::from(delta),
        dx: 0,
        dy: 0,
    }
}

pub fn relative_action(dx: i16, dy: i16) -> MouseAction {
    MouseAction {
        flags: mouse_flags::MOVE,
        data: 0,
        dx: i32::from(dx),
        dy: i32::from(dy),
    }
}

/// Scales relative mouse deltas by a sensitivity multiplier without losing
/// motion. `1.0` is 1:1.
///
/// Rounding each event on its own is lossy in a way that is easy to feel: at
/// 0.5, a slow mouse sending ±1 counts rounds every one of them back to ±1, so
/// half-speed is full speed; at 1.5, ±1 becomes ±2 and slow motion runs a third
/// fast. The fraction each event leaves is carried into the next, so the
/// injected total tracks `sensitivity × Σdelta` to within one count at any
/// speed. It is also what makes the client's own prediction of the pointer
/// (which multiplies by the same gain) come out where the server does.
#[derive(Clone, Copy, Debug, Default)]
pub struct MouseScaler {
    rx: f32,
    ry: f32,
}

impl MouseScaler {
    pub fn new() -> MouseScaler {
        MouseScaler::default()
    }

    /// The whole counts to inject for this delta at `sensitivity`.
    pub fn scale(&mut self, dx: i16, dy: i16, sensitivity: f32) -> (i32, i32) {
        if sensitivity == 1.0 {
            return (i32::from(dx), i32::from(dy));
        }
        let x = carry(&mut self.rx, dx, sensitivity);
        let y = carry(&mut self.ry, dy, sensitivity);
        (x, y)
    }

    /// A relative-move action for this delta.
    pub fn relative_action(&mut self, dx: i16, dy: i16, sensitivity: f32) -> MouseAction {
        let (dx, dy) = self.scale(dx, dy, sensitivity);
        MouseAction {
            flags: mouse_flags::MOVE,
            data: 0,
            dx,
            dy,
        }
    }
}

/// One axis: add the scaled delta to the carried fraction, emit the whole part
/// (toward zero, saturating so a flick cannot wrap), keep the rest.
fn carry(rem: &mut f32, v: i16, sensitivity: f32) -> i32 {
    let total = *rem + f32::from(v) * sensitivity;
    let whole = total.trunc();
    *rem = total - whole;
    whole.clamp(i32::MIN as f32, i32::MAX as f32) as i32
}

/// Windows' pointer-speed multiplier for relative moves with Enhanced Pointer
/// Precision **off**: the Control Panel slider's 1–20 (`SPI_GETMOUSESPEED`),
/// 10 being 1:1. With EPP on there is no such number — the gain depends on the
/// speed of the motion — which is why the server only reports a gain when EPP is
/// off.
pub fn pointer_speed_multiplier(speed: u32) -> f32 {
    const TABLE: [f32; 20] = [
        0.03125, 0.0625, 0.125, 0.25, 0.375, 0.5, 0.625, 0.75, 0.875, 1.0, 1.25, 1.5, 1.75, 2.0,
        2.25, 2.5, 2.75, 3.0, 3.25, 3.5,
    ];
    TABLE[(speed.clamp(1, 20) - 1) as usize]
}

/// Apply a radial deadzone to a stick: if the pair's magnitude is within
/// `deadzone` (0..1) of centre, zero it; otherwise pass it through. `0.0` is a
/// no-op, so the client's own deadzone handling is untouched by default.
pub fn apply_deadzone(x: i16, y: i16, deadzone: f32) -> (i16, i16) {
    if deadzone <= 0.0 {
        return (x, y);
    }
    let threshold = deadzone.clamp(0.0, 1.0) * 32767.0;
    let mag = ((f32::from(x)).hypot(f32::from(y))).abs();
    if mag < threshold { (0, 0) } else { (x, y) }
}

/// Absolute motion across the whole virtual desktop.
///
/// The protocol already carries 0–65535 normalised, which is the range
/// `MOUSEEVENTF_ABSOLUTE` wants, so there is no scaling here — only the flags.
/// `VIRTUALDESK` is what makes the range span every monitor rather than the
/// primary one; without it a second display is unreachable.
pub fn absolute_action(x: u16, y: u16) -> MouseAction {
    MouseAction {
        flags: mouse_flags::MOVE | mouse_flags::ABSOLUTE | mouse_flags::VIRTUALDESK,
        data: 0,
        dx: i32::from(x),
        dy: i32::from(y),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------ extended keys

    #[test]
    fn the_e0_and_e1_prefixes_mean_extended() {
        assert!(is_extended(0xE04D), "right arrow");
        assert!(is_extended(0xE11D), "pause/break");
        assert!(
            !is_extended(0x004D),
            "numpad 4, the twin it becomes without it"
        );
        assert!(!is_extended(0x001E), "the A key");
    }

    #[test]
    fn the_scancode_byte_drops_the_prefix() {
        // SendInput wants the low byte in wScan and the prefix expressed as the
        // flag, not as part of the code.
        assert_eq!(scancode_byte(0xE04D), 0x4D);
        assert_eq!(scancode_byte(0x001E), 0x1E);
    }

    #[test]
    fn the_expected_extended_list_is_a_checklist_not_a_lookup() {
        // It exists to be verified against a real keyboard layout. Its only
        // invariants here are that it is populated and free of duplicates.
        assert!(EXPECTED_EXTENDED.len() > 10);
        let mut keys: Vec<u16> = EXPECTED_EXTENDED.iter().map(|(k, _)| *k).collect();
        keys.sort_unstable();
        let before = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), before, "duplicate in EXPECTED_EXTENDED");
    }

    // ------------------------------------------------------------ modifiers

    #[test]
    fn both_generic_and_sided_modifier_keys_are_recognised() {
        for key in [vk::SHIFT, vk::LSHIFT, vk::RSHIFT] {
            assert_eq!(modifier_of(key), Some(modifiers::SHIFT));
        }
        for key in [vk::CONTROL, vk::LCONTROL, vk::RCONTROL] {
            assert_eq!(modifier_of(key), Some(modifiers::CTRL));
        }
        for key in [vk::MENU, vk::LMENU, vk::RMENU] {
            assert_eq!(modifier_of(key), Some(modifiers::ALT));
        }
        for key in [vk::LWIN, vk::RWIN] {
            assert_eq!(modifier_of(key), Some(modifiers::META));
        }
        assert_eq!(modifier_of(0x41), None, "the A key is not a modifier");
    }

    #[test]
    fn an_ordinary_key_with_no_modifiers_injects_only_itself() {
        let mut m = Modifiers::new();
        assert_eq!(
            m.resolve(0x41, true, 0),
            vec![KeyAction {
                vk: 0x41,
                down: true
            }]
        );
    }

    #[test]
    fn a_modifier_key_is_applied_without_reconciling() {
        // The byte is ambiguous for a modifier's own event — clients differ on
        // whether the bit is set for the key being pressed. Applying directly is
        // correct under either convention.
        let mut m = Modifiers::new();
        assert_eq!(
            m.resolve(vk::LCONTROL, true, modifiers::CTRL),
            vec![KeyAction {
                vk: vk::LCONTROL,
                down: true
            }]
        );
        assert_eq!(m.held(), modifiers::CTRL);

        // And with the other convention, the state still ends up right.
        let mut m = Modifiers::new();
        m.resolve(vk::LCONTROL, true, 0);
        assert_eq!(m.held(), modifiers::CTRL);
    }

    #[test]
    fn a_chord_whose_modifier_was_lost_is_repaired() {
        // The case this exists for. The Ctrl press never arrived, but the key
        // event asserts it, so it is synthesised before the key lands.
        let mut m = Modifiers::new();
        let actions = m.resolve(0x43, true, modifiers::CTRL);
        assert_eq!(
            actions,
            vec![
                KeyAction {
                    vk: vk::LCONTROL,
                    down: true
                },
                KeyAction {
                    vk: 0x43,
                    down: true
                },
            ],
            "Ctrl+C arrived as a bare C"
        );
    }

    #[test]
    fn a_modifier_the_client_has_released_is_lifted() {
        let mut m = Modifiers::new();
        m.resolve(vk::LCONTROL, true, modifiers::CTRL);

        // A later key says nothing is held, so the stuck Ctrl comes up.
        let actions = m.resolve(0x41, true, 0);
        assert_eq!(
            actions,
            vec![
                KeyAction {
                    vk: vk::LCONTROL,
                    down: false
                },
                KeyAction {
                    vk: 0x41,
                    down: true
                },
            ]
        );
        assert_eq!(m.held(), 0);
    }

    #[test]
    fn a_three_key_chord_synthesises_both_modifiers_before_the_key() {
        // Ctrl+Shift+Esc, the worst case for ordering, and the one that has to
        // work or Task Manager never opens.
        let mut m = Modifiers::new();
        let actions = m.resolve(0x1B, true, modifiers::CTRL | modifiers::SHIFT);

        assert_eq!(actions.len(), 3);
        assert!(actions[..2].iter().all(|a| a.down));
        assert_eq!(
            actions[2],
            KeyAction {
                vk: 0x1B,
                down: true
            },
            "the key must come last"
        );
        let synthesised: Vec<u16> = actions[..2].iter().map(|a| a.vk).collect();
        assert!(synthesised.contains(&vk::LSHIFT));
        assert!(synthesised.contains(&vk::LCONTROL));
    }

    #[test]
    fn a_settled_chord_does_not_re_press_its_modifiers() {
        // Repeats within a held chord must not restate the modifier, or a game
        // reading key events sees a stutter.
        let mut m = Modifiers::new();
        m.resolve(vk::LCONTROL, true, modifiers::CTRL);
        let first = m.resolve(0x43, true, modifiers::CTRL);
        assert_eq!(first.len(), 1, "Ctrl already held: {first:?}");

        let second = m.resolve(0x43, false, modifiers::CTRL);
        assert_eq!(second.len(), 1);
    }

    #[test]
    fn releasing_everything_lifts_only_what_is_held() {
        let mut m = Modifiers::new();
        assert!(m.release_all().is_empty(), "nothing held, nothing to lift");

        m.resolve(0x41, true, modifiers::CTRL | modifiers::ALT);
        let released = m.release_all();
        assert_eq!(released.len(), 2);
        assert!(released.iter().all(|a| !a.down));
        assert_eq!(m.held(), 0);
        assert!(m.release_all().is_empty(), "and it is idempotent");
    }

    #[test]
    fn unknown_bits_in_the_byte_are_ignored() {
        // A newer client setting a bit this build does not know must not
        // synthesise a phantom key.
        let mut m = Modifiers::new();
        let actions = m.resolve(0x41, true, 0xF0);
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(m.held(), 0);
    }

    // ------------------------------------------------------------ mouse

    #[test]
    fn x_buttons_are_told_apart_by_mouse_data_not_flags() {
        // They share XDOWN/XUP. A mapping that only sets flags turns X2 into X1,
        // and both are "a side button did the wrong thing".
        let x1 = button_action(MouseButton::X1, true);
        let x2 = button_action(MouseButton::X2, true);
        assert_eq!(x1.flags, x2.flags, "they share a flag by design");
        assert_ne!(x1.data, x2.data, "so mouseData is what distinguishes them");
        assert_eq!(x1.data, xbutton::XBUTTON1);
        assert_eq!(x2.data, xbutton::XBUTTON2);
    }

    #[test]
    fn every_button_maps_to_a_distinct_down_and_up() {
        for button in [
            MouseButton::Left,
            MouseButton::Right,
            MouseButton::Middle,
            MouseButton::X1,
            MouseButton::X2,
        ] {
            let down = button_action(button, true);
            let up = button_action(button, false);
            assert_ne!(down.flags, up.flags, "{button:?} press and release agree");
            assert_eq!(down.data, up.data, "{button:?} changed button mid-click");
        }
    }

    #[test]
    fn wheel_direction_and_axis_are_carried_faithfully() {
        let up = wheel_action(120, false);
        assert_eq!(up.flags, mouse_flags::WHEEL);
        assert_eq!(up.data, 120);

        let down = wheel_action(-120, false);
        assert_eq!(down.data, -120, "the sign is the direction");

        let left = wheel_action(-120, true);
        assert_eq!(left.flags, mouse_flags::HWHEEL);
    }

    #[test]
    fn absolute_motion_spans_the_virtual_desktop() {
        // Without VIRTUALDESK the range covers the primary monitor only, and a
        // second display is simply unreachable.
        let a = absolute_action(32_768, 0);
        assert!(a.flags & mouse_flags::ABSOLUTE != 0);
        assert!(a.flags & mouse_flags::VIRTUALDESK != 0);
        assert!(a.flags & mouse_flags::MOVE != 0);
    }

    #[test]
    fn absolute_coordinates_pass_through_both_extremes() {
        // The protocol already carries 0-65535, which is the range SendInput
        // wants; scaling here would be scaling twice.
        assert_eq!(absolute_action(0, 0).dx, 0);
        assert_eq!(absolute_action(65_535, 65_535).dx, 65_535);
        assert_eq!(absolute_action(65_535, 65_535).dy, 65_535);
    }

    #[test]
    fn relative_motion_keeps_its_sign() {
        let a = relative_action(-300, 42);
        assert_eq!(a.flags, mouse_flags::MOVE);
        assert_eq!((a.dx, a.dy), (-300, 42));
        assert!(a.flags & mouse_flags::ABSOLUTE == 0);
    }

    #[test]
    fn sensitivity_scales_and_1_0_is_identity() {
        let mut m = MouseScaler::new();
        assert_eq!(m.scale(100, -100, 1.0), (100, -100));
        assert_eq!(m.scale(100, 0, 2.0), (200, 0));
        assert_eq!(m.scale(100, 0, 0.5), (50, 0));
        // A big flick scaled up stays exact in i32 rather than wrapping i16.
        assert_eq!(m.scale(30_000, 0, 4.0), (120_000, 0));
        let a = m.relative_action(-50, 20, 2.0);
        assert_eq!(a.flags, mouse_flags::MOVE);
        assert_eq!((a.dx, a.dy), (-100, 40));
    }

    #[test]
    fn slow_motion_keeps_its_fraction() {
        // A slow mouse sends ±1. Rounded per event, 0.5 gave ±1 (full speed)
        // and 1.5 gave ±2; carried, the total is the scaled total.
        for sensitivity in [0.5f32, 0.75, 1.5, 2.25] {
            for sign in [1i16, -1] {
                let mut m = MouseScaler::new();
                let mut total = 0i32;
                for _ in 0..1000 {
                    total += m.scale(sign, 0, sensitivity).0;
                }
                let want = f32::from(sign) * 1000.0 * sensitivity;
                assert!(
                    (total as f32 - want).abs() <= 1.0,
                    "{sensitivity} x {sign}: {total} vs {want}"
                );
            }
        }
    }

    #[test]
    fn the_axes_carry_separately_and_a_reversal_is_symmetric() {
        let mut m = MouseScaler::new();
        assert_eq!(m.scale(1, -1, 0.5), (0, 0));
        assert_eq!(m.scale(1, -1, 0.5), (1, -1));
        // Back the other way: the carried halves cancel rather than lurch.
        assert_eq!(m.scale(-1, 1, 0.5), (0, 0));
        assert_eq!(m.scale(-1, 1, 0.5), (-1, 1));
    }

    #[test]
    fn pointer_speed_ten_is_one_to_one() {
        assert_eq!(pointer_speed_multiplier(10), 1.0);
        assert_eq!(pointer_speed_multiplier(6), 0.5);
        assert_eq!(pointer_speed_multiplier(14), 2.0);
        assert_eq!(pointer_speed_multiplier(1), 0.03125);
        assert_eq!(pointer_speed_multiplier(20), 3.5);
        // Out-of-range readings clamp rather than index out of bounds.
        assert_eq!(pointer_speed_multiplier(0), 0.03125);
        assert_eq!(pointer_speed_multiplier(99), 3.5);
    }

    #[test]
    fn deadzone_zeroes_only_within_the_radius() {
        // 0 is a no-op — the client's own deadzone is untouched.
        assert_eq!(apply_deadzone(1000, 0, 0.0), (1000, 0));
        // Inside a 10% radius (≈3276) is zeroed; outside passes through.
        assert_eq!(apply_deadzone(2000, 0, 0.10), (0, 0));
        assert_eq!(apply_deadzone(30_000, 0, 0.10), (30_000, 0));
    }
}

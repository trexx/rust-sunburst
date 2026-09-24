// SPDX-License-Identifier: GPL-2.0-or-later

//! Mapping Android input to the wire protocol — pure, host-tested, no platform.
//!
//! Kotlin forwards raw Android events as [`ClientInput`]; [`InputAccumulator`]
//! maps them to [`InputEvent`]s the server understands: Android keycodes to
//! Windows virtual-key codes, gamepad keycodes and axes to an XInput-shaped
//! [`GamepadState`], mouse deltas and wheel to their events. Keyboard/gamepad
//! routing is Kotlin's job (it knows the event source); this maps each stream.

use sunburst_core::proto::input::{
    GamepadState, InputEvent, MAX_PADS, MouseButton, MouseMotion, buttons,
};
use sunburst_input::keymap::modifiers;

/// A raw Android event, before mapping. Floats are Android's normalised axis
/// values (sticks −1..1, triggers/hats 0..1 or −1..1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ClientInput {
    Key {
        code: i32,
        down: bool,
        meta: i32,
    },
    /// A relative mouse move already in whole counts, numbered by the UI
    /// thread (which predicts the cursor from it) as `seq`.
    MouseRel {
        seq: u32,
        dx: i16,
        dy: i16,
    },
    MouseButton {
        code: i32,
        down: bool,
    },
    Wheel {
        delta: f32,
        horizontal: bool,
    },
    PadButton {
        code: i32,
        down: bool,
    },
    PadAxis {
        lx: f32,
        ly: f32,
        rx: f32,
        ry: f32,
        lt: f32,
        rt: f32,
        hat_x: f32,
        hat_y: f32,
    },
    /// A fully-decoded pad from the GIP bridge (an Xbox pad on the adapter or
    /// wired USB). Unlike `PadButton`/`PadAxis`, which arrive as incremental
    /// Android View events for the single TV-native pad, this carries the whole
    /// `GamepadState` for a specific `index`, so the accumulator just stamps the
    /// index and forwards it.
    Pad {
        index: u8,
        state: GamepadState,
    },
}

// --- Android KeyEvent keycodes (stable platform constants) -------------------
const KC_A: i32 = 29;
const KC_Z: i32 = 54;
const KC_0: i32 = 7;
const KC_9: i32 = 16;
const KC_F1: i32 = 131;
const KC_F12: i32 = 142;

/// An Android keyboard keycode to a Windows virtual-key code.
pub fn android_key_to_vk(code: i32) -> Option<u16> {
    // Letters and digits are contiguous in both encodings.
    if (KC_A..=KC_Z).contains(&code) {
        return Some(0x41 + (code - KC_A) as u16); // 'A'..'Z'
    }
    if (KC_0..=KC_9).contains(&code) {
        return Some(0x30 + (code - KC_0) as u16); // '0'..'9'
    }
    if (KC_F1..=KC_F12).contains(&code) {
        return Some(0x70 + (code - KC_F1) as u16); // VK_F1..VK_F12
    }
    Some(match code {
        62 => 0x20,        // SPACE
        66 => 0x0D,        // ENTER
        111 => 0x1B,       // ESCAPE
        61 => 0x09,        // TAB
        67 => 0x08,        // DEL -> Backspace
        112 => 0x2E,       // FORWARD_DEL -> Delete
        59 | 60 => 0x10,   // SHIFT_LEFT/RIGHT
        113 | 114 => 0x11, // CTRL_LEFT/RIGHT
        57 | 58 => 0x12,   // ALT_LEFT/RIGHT
        117 | 118 => 0x5B, // META_LEFT/RIGHT -> Left Win
        19 => 0x26,        // DPAD_UP -> Up (keyboard arrows arrive as DPAD_*)
        20 => 0x28,        // DPAD_DOWN -> Down
        21 => 0x25,        // DPAD_LEFT -> Left
        22 => 0x27,        // DPAD_RIGHT -> Right
        92 => 0x21,        // PAGE_UP
        93 => 0x22,        // PAGE_DOWN
        122 => 0x24,       // MOVE_HOME -> Home
        123 => 0x23,       // MOVE_END -> End
        124 => 0x2D,       // INSERT
        68 => 0xC0,        // GRAVE
        69 => 0xBD,        // MINUS
        70 => 0xBB,        // EQUALS
        71 => 0xDB,        // LEFT_BRACKET
        72 => 0xDD,        // RIGHT_BRACKET
        73 => 0xDC,        // BACKSLASH
        74 => 0xBA,        // SEMICOLON
        75 => 0xDE,        // APOSTROPHE
        55 => 0xBC,        // COMMA
        56 => 0xBE,        // PERIOD
        76 => 0xBF,        // SLASH
        _ => return None,
    })
}

/// Android `KeyEvent` meta-state bits to the protocol's modifier bits.
pub fn android_meta_to_modifiers(meta: i32) -> u8 {
    const META_SHIFT_ON: i32 = 0x1;
    const META_ALT_ON: i32 = 0x2;
    const META_CTRL_ON: i32 = 0x1000;
    const META_META_ON: i32 = 0x10000;
    let mut m = 0u8;
    if meta & META_SHIFT_ON != 0 {
        m |= modifiers::SHIFT;
    }
    if meta & META_CTRL_ON != 0 {
        m |= modifiers::CTRL;
    }
    if meta & META_ALT_ON != 0 {
        m |= modifiers::ALT;
    }
    if meta & META_META_ON != 0 {
        m |= modifiers::META;
    }
    m
}

/// Android `MotionEvent` mouse button index to a [`MouseButton`].
pub fn android_mouse_button(code: i32) -> Option<MouseButton> {
    // BUTTON_PRIMARY=1, SECONDARY=2, TERTIARY=4, BACK=8, FORWARD=16.
    Some(match code {
        1 => MouseButton::Left,
        2 => MouseButton::Right,
        4 => MouseButton::Middle,
        8 => MouseButton::X1,
        16 => MouseButton::X2,
        _ => return None,
    })
}

/// An Android gamepad keycode to its XInput button bit.
pub fn android_button_to_xinput(code: i32) -> Option<u32> {
    Some(match code {
        96 => buttons::A,               // BUTTON_A
        97 => buttons::B,               // BUTTON_B
        99 => buttons::X,               // BUTTON_X
        100 => buttons::Y,              // BUTTON_Y
        102 => buttons::LEFT_SHOULDER,  // BUTTON_L1
        103 => buttons::RIGHT_SHOULDER, // BUTTON_R1
        106 => buttons::LEFT_THUMB,     // BUTTON_THUMBL
        107 => buttons::RIGHT_THUMB,    // BUTTON_THUMBR
        108 => buttons::START,          // BUTTON_START
        109 => buttons::BACK,           // BUTTON_SELECT
        110 => buttons::GUIDE,          // BUTTON_MODE
        19 => buttons::DPAD_UP,         // DPAD_UP (from a gamepad source)
        20 => buttons::DPAD_DOWN,
        21 => buttons::DPAD_LEFT,
        22 => buttons::DPAD_RIGHT,
        _ => return None,
    })
}

/// A normalised stick axis (−1..1) to the wire's i16 range.
pub fn stick_to_i16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

/// A normalised trigger axis (0..1) to the wire's 0..255.
pub fn trigger_to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0) as u8
}

impl ClientInput {
    /// The sequence number the UI thread already gave this input, if any.
    pub fn seq(&self) -> Option<u32> {
        match self {
            ClientInput::MouseRel { seq, .. } => Some(*seq),
            _ => None,
        }
    }
}

/// Accumulates gamepad state across button/axis events so each change emits a
/// full [`GamepadState`]; other events map one-to-one.
///
/// One slot per pad index. The Android-View pad (`PadButton`/`PadAxis`, a
/// controller paired directly to the TV) accumulates into slot 0; the GIP bridge
/// delivers whole states via [`ClientInput::Pad`] into their own slots, so up to
/// [`MAX_PADS`] controllers stay independent.
#[derive(Debug)]
pub struct InputAccumulator {
    pads: [GamepadState; MAX_PADS as usize],
}

impl Default for InputAccumulator {
    fn default() -> InputAccumulator {
        let mut pads = [GamepadState::default(); MAX_PADS as usize];
        for (i, p) in pads.iter_mut().enumerate() {
            p.pad_index = i as u8;
        }
        InputAccumulator { pads }
    }
}

impl InputAccumulator {
    pub fn new() -> InputAccumulator {
        InputAccumulator::default()
    }

    /// Map one raw event, returning the wire event to send (if any).
    pub fn apply(&mut self, input: ClientInput) -> Option<InputEvent> {
        match input {
            ClientInput::Key { code, down, meta } => {
                let vk = android_key_to_vk(code)?;
                let modifiers = android_meta_to_modifiers(meta);
                Some(if down {
                    InputEvent::KeyDown { vk, modifiers }
                } else {
                    InputEvent::KeyUp { vk, modifiers }
                })
            }
            ClientInput::MouseRel { dx, dy, .. } => {
                Some(InputEvent::MouseMove(MouseMotion::Relative { dx, dy }))
            }
            ClientInput::MouseButton { code, down } => {
                let button = android_mouse_button(code)?;
                Some(InputEvent::MouseButton { button, down })
            }
            ClientInput::Wheel { delta, horizontal } => Some(InputEvent::MouseWheel {
                // Android scroll is in "wheel clicks"; the wire unit is WHEEL_DELTA (120).
                delta: (delta * 120.0).clamp(i16::MIN as f32, i16::MAX as f32) as i16,
                horizontal,
            }),
            ClientInput::PadButton { code, down } => {
                let bit = android_button_to_xinput(code)?;
                let pad = &mut self.pads[0];
                if down {
                    pad.buttons |= bit;
                } else {
                    pad.buttons &= !bit;
                }
                Some(InputEvent::Gamepad(*pad))
            }
            ClientInput::PadAxis {
                lx,
                ly,
                rx,
                ry,
                lt,
                rt,
                hat_x,
                hat_y,
            } => {
                let pad = &mut self.pads[0];
                pad.lx = stick_to_i16(lx);
                // Android Y is down-positive; XInput up-positive.
                pad.ly = stick_to_i16(-ly);
                pad.rx = stick_to_i16(rx);
                pad.ry = stick_to_i16(-ry);
                pad.lt = trigger_to_u8(lt);
                pad.rt = trigger_to_u8(rt);
                // The hat axes carry the d-pad on many pads; fold into the bits.
                let dpad = buttons::DPAD_UP
                    | buttons::DPAD_DOWN
                    | buttons::DPAD_LEFT
                    | buttons::DPAD_RIGHT;
                pad.buttons &= !dpad;
                if hat_y < -0.5 {
                    pad.buttons |= buttons::DPAD_UP;
                } else if hat_y > 0.5 {
                    pad.buttons |= buttons::DPAD_DOWN;
                }
                if hat_x < -0.5 {
                    pad.buttons |= buttons::DPAD_LEFT;
                } else if hat_x > 0.5 {
                    pad.buttons |= buttons::DPAD_RIGHT;
                }
                Some(InputEvent::Gamepad(*pad))
            }
            ClientInput::Pad { index, state } => {
                // The bridge already decoded a whole state; a slot out of range
                // is dropped rather than clamped, so a bug cannot cross pads.
                let slot = self.pads.get_mut(index as usize)?;
                *slot = state;
                slot.pad_index = index;
                Some(InputEvent::Gamepad(*slot))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_digits_and_named_keys_map_to_vk() {
        assert_eq!(android_key_to_vk(29), Some(0x41)); // A
        assert_eq!(android_key_to_vk(54), Some(0x5A)); // Z
        assert_eq!(android_key_to_vk(7), Some(0x30)); // 0
        assert_eq!(android_key_to_vk(16), Some(0x39)); // 9
        assert_eq!(android_key_to_vk(66), Some(0x0D)); // ENTER
        assert_eq!(android_key_to_vk(131), Some(0x70)); // F1
        assert_eq!(android_key_to_vk(142), Some(0x7B)); // F12
        assert_eq!(android_key_to_vk(4), None); // BACK, not a key
    }

    #[test]
    fn meta_maps_to_modifier_bits() {
        assert_eq!(android_meta_to_modifiers(0), 0);
        assert_eq!(android_meta_to_modifiers(0x1), modifiers::SHIFT);
        assert_eq!(android_meta_to_modifiers(0x1000), modifiers::CTRL);
        assert_eq!(
            android_meta_to_modifiers(0x1 | 0x1000),
            modifiers::SHIFT | modifiers::CTRL
        );
    }

    #[test]
    fn a_key_down_then_up_round_trips() {
        let mut acc = InputAccumulator::new();
        assert_eq!(
            acc.apply(ClientInput::Key {
                code: 29,
                down: true,
                meta: 0x1000
            }),
            Some(InputEvent::KeyDown {
                vk: 0x41,
                modifiers: modifiers::CTRL
            })
        );
        assert_eq!(
            acc.apply(ClientInput::Key {
                code: 29,
                down: false,
                meta: 0
            }),
            Some(InputEvent::KeyUp {
                vk: 0x41,
                modifiers: 0
            })
        );
    }

    #[test]
    fn gamepad_buttons_accumulate() {
        let mut acc = InputAccumulator::new();
        let a = acc
            .apply(ClientInput::PadButton {
                code: 96,
                down: true,
            })
            .unwrap();
        let InputEvent::Gamepad(s) = a else { panic!() };
        assert_eq!(s.buttons & buttons::A, buttons::A);
        // Pressing B keeps A held.
        let b = acc
            .apply(ClientInput::PadButton {
                code: 97,
                down: true,
            })
            .unwrap();
        let InputEvent::Gamepad(s) = b else { panic!() };
        assert_eq!(
            s.buttons & (buttons::A | buttons::B),
            buttons::A | buttons::B
        );
        // Releasing A leaves B.
        let c = acc
            .apply(ClientInput::PadButton {
                code: 96,
                down: false,
            })
            .unwrap();
        let InputEvent::Gamepad(s) = c else { panic!() };
        assert_eq!(s.buttons & buttons::A, 0);
        assert_eq!(s.buttons & buttons::B, buttons::B);
    }

    #[test]
    fn axes_scale_and_invert_y_and_fold_the_hat() {
        let mut acc = InputAccumulator::new();
        let e = acc
            .apply(ClientInput::PadAxis {
                lx: 1.0,
                ly: 1.0,
                rx: -1.0,
                ry: 0.0,
                lt: 0.5,
                rt: 1.0,
                hat_x: -1.0,
                hat_y: -1.0,
            })
            .unwrap();
        let InputEvent::Gamepad(s) = e else { panic!() };
        assert_eq!(s.lx, i16::MAX);
        assert_eq!(s.ly, i16::MIN + 1); // -1.0 * MAX, inverted from +1.0
        assert_eq!(s.rx, i16::MIN + 1);
        assert_eq!(s.rt, 255);
        assert!((s.lt as i32 - 127).abs() <= 1);
        assert_eq!(s.buttons & buttons::DPAD_LEFT, buttons::DPAD_LEFT);
        assert_eq!(s.buttons & buttons::DPAD_UP, buttons::DPAD_UP);
    }

    #[test]
    fn bridge_pads_are_independent_and_keep_their_index() {
        let mut acc = InputAccumulator::new();

        // Two GIP-decoded pads at different indices.
        let mut p1 = GamepadState {
            buttons: buttons::A,
            lt: 200,
            ..Default::default()
        };
        p1.pad_index = 99; // deliberately wrong; the accumulator must restamp it
        let e1 = acc.apply(ClientInput::Pad {
            index: 1,
            state: p1,
        });
        let Some(InputEvent::Gamepad(s1)) = e1 else {
            panic!("expected a gamepad event")
        };
        assert_eq!(s1.pad_index, 1, "the slot index wins over the payload's");
        assert_eq!(s1.buttons & buttons::A, buttons::A);

        let p3 = GamepadState {
            buttons: buttons::B,
            ..Default::default()
        };
        let e3 = acc.apply(ClientInput::Pad {
            index: 3,
            state: p3,
        });
        let Some(InputEvent::Gamepad(s3)) = e3 else {
            panic!("expected a gamepad event")
        };
        assert_eq!(s3.pad_index, 3);
        assert_eq!(s3.buttons, buttons::B, "pad 3 is not polluted by pad 1");

        // The TV-native path is pad 0, distinct from the bridge pads.
        let e0 = acc
            .apply(ClientInput::PadButton {
                code: 96,
                down: true,
            })
            .unwrap();
        let InputEvent::Gamepad(s0) = e0 else {
            panic!()
        };
        assert_eq!(s0.pad_index, 0);

        // An index past MAX_PADS is dropped, never clamped onto another pad.
        assert_eq!(
            acc.apply(ClientInput::Pad {
                index: MAX_PADS,
                state: GamepadState::default(),
            }),
            None
        );
    }

    #[test]
    fn mouse_wheel_uses_wheel_delta_units() {
        let mut acc = InputAccumulator::new();
        assert_eq!(
            acc.apply(ClientInput::Wheel {
                delta: 1.0,
                horizontal: false
            }),
            Some(InputEvent::MouseWheel {
                delta: 120,
                horizontal: false
            })
        );
    }
}

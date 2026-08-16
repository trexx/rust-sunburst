// SPDX-License-Identifier: GPL-2.0-or-later

//! Input event payloads.
//!
//! Encoded into caller-supplied buffers rather than returning a `Vec`: the
//! client sends these from the input path, and an allocation per keystroke is
//! the kind of thing CLAUDE.md's first hot-path rule exists to prevent.

/// Simultaneous pads. Four, because that is what the Xbox Wireless Adapter
/// supports and there is no reason to allow an index the server cannot plug.
pub const MAX_PADS: u8 = 4;

/// Largest encoded input body: `input_seq` + kind + the biggest payload.
pub const MAX_INPUT_BODY: usize = 4 + 1 + 13;

/// X360 button bits, as XInput defines them.
///
/// Pinned here rather than left to each end, because a client and server that
/// disagree produce a controller where two buttons are swapped — which reads as
/// a game bug, not a protocol bug.
pub mod buttons {
    pub const DPAD_UP: u16 = 0x0001;
    pub const DPAD_DOWN: u16 = 0x0002;
    pub const DPAD_LEFT: u16 = 0x0004;
    pub const DPAD_RIGHT: u16 = 0x0008;
    pub const START: u16 = 0x0010;
    pub const BACK: u16 = 0x0020;
    pub const LEFT_THUMB: u16 = 0x0040;
    pub const RIGHT_THUMB: u16 = 0x0080;
    pub const LEFT_SHOULDER: u16 = 0x0100;
    pub const RIGHT_SHOULDER: u16 = 0x0200;
    /// Not in the public XInput headers; exposed by `XInputGetStateEx`.
    pub const GUIDE: u16 = 0x0400;
    pub const A: u16 = 0x1000;
    pub const B: u16 = 0x2000;
    pub const X: u16 = 0x4000;
    pub const Y: u16 = 0x8000;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum InputKind {
    Gamepad = 0,
    KeyDown = 1,
    KeyUp = 2,
    MouseMove = 3,
    MouseButton = 4,
    MouseWheel = 5,
}

/// One pad's complete state. Maps directly onto a ViGEm X360 report.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct GamepadState {
    pub pad_index: u8,
    pub buttons: u16,
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
    pub lt: u8,
    pub rt: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseMotion {
    /// Deltas, for captured-pointer mode.
    ///
    /// Enhanced Pointer Precision applies an acceleration curve to injected
    /// relative motion on the server; see CLAUDE.md. Nothing to do about it
    /// here, but this is the payload it distorts.
    Relative { dx: i16, dy: i16 },
    /// Normalised 0–65535 across the virtual desktop.
    Absolute { x: u16, y: u16 },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum MouseButton {
    Left = 0,
    Right = 1,
    Middle = 2,
    X1 = 3,
    X2 = 4,
}

impl MouseButton {
    const fn from_u8(v: u8) -> Option<MouseButton> {
        Some(match v {
            0 => MouseButton::Left,
            1 => MouseButton::Right,
            2 => MouseButton::Middle,
            3 => MouseButton::X1,
            4 => MouseButton::X2,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputEvent {
    Gamepad(GamepadState),
    /// Windows virtual-key code plus a modifier bitfield.
    ///
    /// A VK, deliberately, not Android's `getScanCode()` — those are Linux evdev
    /// codes and the mapping is not clean. The server derives the scancode with
    /// `MapVirtualKeyW`, which is what games reading raw input actually see.
    KeyDown {
        vk: u16,
        modifiers: u8,
    },
    KeyUp {
        vk: u16,
        modifiers: u8,
    },
    MouseMove(MouseMotion),
    MouseButton {
        button: MouseButton,
        down: bool,
    },
    /// `delta` in units of `WHEEL_DELTA` (120).
    MouseWheel {
        delta: i16,
        horizontal: bool,
    },
}

impl InputEvent {
    pub const fn kind(&self) -> InputKind {
        match self {
            InputEvent::Gamepad(_) => InputKind::Gamepad,
            InputEvent::KeyDown { .. } => InputKind::KeyDown,
            InputEvent::KeyUp { .. } => InputKind::KeyUp,
            InputEvent::MouseMove(_) => InputKind::MouseMove,
            InputEvent::MouseButton { .. } => InputKind::MouseButton,
            InputEvent::MouseWheel { .. } => InputKind::MouseWheel,
        }
    }
}

/// An input packet body: everything between the common header and the MAC.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InputPacket {
    pub input_seq: u32,
    pub event: InputEvent,
}

impl InputPacket {
    /// Encode into `out`, returning the number of bytes written.
    ///
    /// `None` if `out` is too small; [`MAX_INPUT_BODY`] is always enough.
    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < MAX_INPUT_BODY {
            return None;
        }
        out[0..4].copy_from_slice(&self.input_seq.to_le_bytes());
        out[4] = self.event.kind() as u8;
        let body = &mut out[5..];

        let n = match self.event {
            InputEvent::Gamepad(g) => {
                body[0] = g.pad_index;
                body[1..3].copy_from_slice(&g.buttons.to_le_bytes());
                body[3..5].copy_from_slice(&g.lx.to_le_bytes());
                body[5..7].copy_from_slice(&g.ly.to_le_bytes());
                body[7..9].copy_from_slice(&g.rx.to_le_bytes());
                body[9..11].copy_from_slice(&g.ry.to_le_bytes());
                body[11] = g.lt;
                body[12] = g.rt;
                13
            }
            InputEvent::KeyDown { vk, modifiers } | InputEvent::KeyUp { vk, modifiers } => {
                body[0..2].copy_from_slice(&vk.to_le_bytes());
                body[2] = modifiers;
                3
            }
            InputEvent::MouseMove(m) => {
                match m {
                    MouseMotion::Relative { dx, dy } => {
                        body[0] = 0;
                        body[1..3].copy_from_slice(&dx.to_le_bytes());
                        body[3..5].copy_from_slice(&dy.to_le_bytes());
                    }
                    MouseMotion::Absolute { x, y } => {
                        body[0] = 1;
                        body[1..3].copy_from_slice(&x.to_le_bytes());
                        body[3..5].copy_from_slice(&y.to_le_bytes());
                    }
                }
                5
            }
            InputEvent::MouseButton { button, down } => {
                body[0] = button as u8;
                body[1] = u8::from(down);
                2
            }
            InputEvent::MouseWheel { delta, horizontal } => {
                body[0..2].copy_from_slice(&delta.to_le_bytes());
                body[2] = u8::from(horizontal);
                3
            }
        };
        Some(5 + n)
    }

    /// Decode a body. `None` on anything malformed — a short buffer, an unknown
    /// kind, an out-of-range pad or button.
    pub fn decode(buf: &[u8]) -> Option<InputPacket> {
        if buf.len() < 5 {
            return None;
        }
        let input_seq = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let kind = buf[4];
        let body = &buf[5..];

        let le16 = |o: usize| u16::from_le_bytes([body[o], body[o + 1]]);
        let lei16 = |o: usize| i16::from_le_bytes([body[o], body[o + 1]]);

        let event = match kind {
            0 => {
                if body.len() < 13 {
                    return None;
                }
                let pad_index = body[0];
                if pad_index >= MAX_PADS {
                    // Refused at the boundary rather than trusted and used to
                    // index a ViGEm target array further in.
                    return None;
                }
                InputEvent::Gamepad(GamepadState {
                    pad_index,
                    buttons: le16(1),
                    lx: lei16(3),
                    ly: lei16(5),
                    rx: lei16(7),
                    ry: lei16(9),
                    lt: body[11],
                    rt: body[12],
                })
            }
            1 | 2 => {
                if body.len() < 3 {
                    return None;
                }
                let (vk, modifiers) = (le16(0), body[2]);
                if kind == 1 {
                    InputEvent::KeyDown { vk, modifiers }
                } else {
                    InputEvent::KeyUp { vk, modifiers }
                }
            }
            3 => {
                if body.len() < 5 {
                    return None;
                }
                InputEvent::MouseMove(match body[0] {
                    0 => MouseMotion::Relative {
                        dx: lei16(1),
                        dy: lei16(3),
                    },
                    1 => MouseMotion::Absolute {
                        x: le16(1),
                        y: le16(3),
                    },
                    _ => return None,
                })
            }
            4 => {
                if body.len() < 2 {
                    return None;
                }
                InputEvent::MouseButton {
                    button: MouseButton::from_u8(body[0])?,
                    down: body[1] != 0,
                }
            }
            5 => {
                if body.len() < 3 {
                    return None;
                }
                InputEvent::MouseWheel {
                    delta: lei16(0),
                    horizontal: body[2] != 0,
                }
            }
            _ => return None,
        };

        Some(InputPacket { input_seq, event })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(event: InputEvent) {
        let packet = InputPacket {
            input_seq: 0x1234_5678,
            event,
        };
        let mut buf = [0u8; MAX_INPUT_BODY];
        let n = packet.encode(&mut buf).expect("should encode");
        assert_eq!(
            InputPacket::decode(&buf[..n]),
            Some(packet),
            "round trip failed for {event:?}"
        );
    }

    #[test]
    fn every_event_round_trips() {
        round_trip(InputEvent::Gamepad(GamepadState {
            pad_index: 3,
            buttons: buttons::A | buttons::DPAD_LEFT | buttons::GUIDE,
            lx: -32768,
            ly: 32767,
            rx: -1,
            ry: 1,
            lt: 0,
            rt: 255,
        }));
        round_trip(InputEvent::KeyDown {
            vk: 0x1B,
            modifiers: 0x05,
        });
        round_trip(InputEvent::KeyUp {
            vk: 0xFF,
            modifiers: 0,
        });
        round_trip(InputEvent::MouseMove(MouseMotion::Relative {
            dx: -300,
            dy: 42,
        }));
        round_trip(InputEvent::MouseMove(MouseMotion::Absolute {
            x: 65535,
            y: 0,
        }));
        round_trip(InputEvent::MouseButton {
            button: MouseButton::X2,
            down: true,
        });
        round_trip(InputEvent::MouseWheel {
            delta: -120,
            horizontal: true,
        });
    }

    #[test]
    fn keydown_and_keyup_are_distinguishable() {
        // They share a payload shape, so a decoder that keyed off length alone
        // would confuse them — and a stuck key is the visible result.
        let mut buf = [0u8; MAX_INPUT_BODY];
        let down = InputPacket {
            input_seq: 1,
            event: InputEvent::KeyDown {
                vk: 65,
                modifiers: 0,
            },
        };
        let n = down.encode(&mut buf).unwrap();
        assert!(matches!(
            InputPacket::decode(&buf[..n]).unwrap().event,
            InputEvent::KeyDown { .. }
        ));

        let up = InputPacket {
            input_seq: 1,
            event: InputEvent::KeyUp {
                vk: 65,
                modifiers: 0,
            },
        };
        let n = up.encode(&mut buf).unwrap();
        assert!(matches!(
            InputPacket::decode(&buf[..n]).unwrap().event,
            InputEvent::KeyUp { .. }
        ));
    }

    #[test]
    fn relative_and_absolute_motion_are_distinguishable() {
        // Same five bytes, different meaning. Confusing them puts the pointer in
        // the corner of the screen rather than moving it.
        let mut buf = [0u8; MAX_INPUT_BODY];
        InputPacket {
            input_seq: 1,
            event: InputEvent::MouseMove(MouseMotion::Relative { dx: 1, dy: 1 }),
        }
        .encode(&mut buf)
        .unwrap();
        assert_eq!(buf[5], 0);

        InputPacket {
            input_seq: 1,
            event: InputEvent::MouseMove(MouseMotion::Absolute { x: 1, y: 1 }),
        }
        .encode(&mut buf)
        .unwrap();
        assert_eq!(buf[5], 1);
    }

    #[test]
    fn gamepad_byte_layout_matches_the_specification() {
        let mut buf = [0u8; MAX_INPUT_BODY];
        let n = InputPacket {
            input_seq: 1,
            event: InputEvent::Gamepad(GamepadState {
                pad_index: 2,
                buttons: 0xABCD,
                lx: 0x0102,
                ly: 0x0304,
                rx: 0x0506,
                ry: 0x0708,
                lt: 0x11,
                rt: 0x22,
            }),
        }
        .encode(&mut buf)
        .unwrap();

        assert_eq!(n, 18);
        assert_eq!(&buf[..5], &[0x01, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(
            &buf[5..18],
            &[
                0x02, // pad_index
                0xCD, 0xAB, // buttons
                0x02, 0x01, 0x04, 0x03, 0x06, 0x05, 0x08, 0x07, // sticks
                0x11, 0x22, // triggers
            ]
        );
    }

    #[test]
    fn a_pad_index_the_server_cannot_plug_is_refused() {
        let mut buf = [0u8; MAX_INPUT_BODY];
        let n = InputPacket {
            input_seq: 1,
            event: InputEvent::Gamepad(GamepadState {
                pad_index: 0,
                ..Default::default()
            }),
        }
        .encode(&mut buf)
        .unwrap();

        for bad in [MAX_PADS, MAX_PADS + 1, 255] {
            buf[5] = bad;
            assert_eq!(
                InputPacket::decode(&buf[..n]),
                None,
                "pad_index {bad} should be refused at the boundary"
            );
        }
    }

    #[test]
    fn unknown_kinds_and_modes_are_refused() {
        let mut buf = [0u8; MAX_INPUT_BODY];
        buf[4] = 6; // one past MouseWheel
        assert_eq!(InputPacket::decode(&buf), None);

        buf[4] = 3; // MouseMove
        buf[5] = 2; // neither relative nor absolute
        assert_eq!(InputPacket::decode(&buf), None);

        buf[4] = 4; // MouseButton
        buf[5] = 5; // one past X2
        assert_eq!(InputPacket::decode(&buf), None);
    }

    #[test]
    fn truncated_bodies_are_refused_rather_than_panicking() {
        // A hostile or corrupt packet must not index past the end.
        let mut buf = [0u8; MAX_INPUT_BODY];
        let n = InputPacket {
            input_seq: 1,
            event: InputEvent::Gamepad(GamepadState::default()),
        }
        .encode(&mut buf)
        .unwrap();

        for len in 0..n {
            assert_eq!(
                InputPacket::decode(&buf[..len]),
                None,
                "a {len}-byte gamepad body should be refused"
            );
        }
    }

    #[test]
    fn encode_refuses_a_buffer_it_could_overrun() {
        let packet = InputPacket {
            input_seq: 1,
            event: InputEvent::Gamepad(GamepadState::default()),
        };
        let mut small = [0u8; MAX_INPUT_BODY - 1];
        assert_eq!(packet.encode(&mut small), None);
    }

    #[test]
    fn max_input_body_covers_the_largest_event() {
        // If a payload ever outgrows this, the buffers on both ends are wrong
        // and encode would start refusing at runtime instead.
        let mut buf = [0u8; MAX_INPUT_BODY];
        let n = InputPacket {
            input_seq: 0,
            event: InputEvent::Gamepad(GamepadState::default()),
        }
        .encode(&mut buf)
        .unwrap();
        assert_eq!(n, MAX_INPUT_BODY, "gamepad should be the largest payload");
    }
}

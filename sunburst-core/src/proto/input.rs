// SPDX-License-Identifier: GPL-2.0-or-later

//! Input event payloads.
//!
//! Encoded into caller-supplied buffers rather than returning a `Vec`: the
//! client sends these from the input path, and an allocation per keystroke is
//! the kind of thing CLAUDE.md's first hot-path rule exists to prevent.

/// Simultaneous pads. Four, because that is what the Xbox Wireless Adapter
/// supports and there is no reason to allow an index the server cannot plug.
pub const MAX_PADS: u8 = 4;

/// Gamepad body layout, in bytes (little-endian, presence-flagged). The core is
/// always present; a presence mask (see [`presence`]) gates the rest. See
/// PROTOCOL.md — this and the document must agree.
const GAMEPAD_CORE: usize = 16; // pad_index, presence, buttons u32, 4×i16, 2×u8
const GAMEPAD_IMU: usize = 16; // gyro 3×i16 + accel 3×i16 + sensor_timestamp u32
const GAMEPAD_TOUCHPAD: usize = 10; // two fingers, 5 bytes each
const GAMEPAD_BATTERY: usize = 2; // level u8 + flags u8
const GAMEPAD_MAX_BODY: usize = GAMEPAD_CORE + GAMEPAD_IMU + GAMEPAD_TOUCHPAD + GAMEPAD_BATTERY;

/// Bytes before a body: `input_seq` (u32) + `input_kind` (u8).
const INPUT_PREFIX: usize = 5;

/// Largest encoded input body: the prefix plus a fully-populated gamepad, which
/// is the biggest payload of any kind.
pub const MAX_INPUT_BODY: usize = INPUT_PREFIX + GAMEPAD_MAX_BODY;

/// Presence-mask bits: which optional gamepad sections a packet carries. A pad's
/// *capability* to send a section is announced once at connect
/// (`PadConnected.capabilities`); this says which are in *this* packet.
pub mod presence {
    pub const IMU: u8 = 1 << 0;
    pub const TOUCHPAD: u8 = 1 << 1;
    pub const BATTERY: u8 = 1 << 2;
}

/// Battery-section flag bits.
pub mod battery_flags {
    pub const CHARGING: u8 = 1 << 0;
    pub const FULL: u8 = 1 << 1;
    pub const MIC_MUTED: u8 = 1 << 2;
    pub const HEADPHONES: u8 = 1 << 3;
}

/// Canonical button bits. The low 16 are XInput's own values, so the X360
/// fallback maps them straight through; the bits above 16 are controls XInput
/// has no name for, which the server maps to HMButton for the HIDMaestro codec
/// and the X360 fallback simply drops.
///
/// Pinned here rather than left to each end, because a client and server that
/// disagree produce a controller where two buttons are swapped — which reads as
/// a game bug, not a protocol bug.
///
/// The d-pad lives in bits 0–3 (XInput's four direction bits); the server
/// derives the codec's hat octant from them, so there is no separate hat field.
pub mod buttons {
    // XInput's own bit values, widened to u32.
    pub const DPAD_UP: u32 = 0x0001;
    pub const DPAD_DOWN: u32 = 0x0002;
    pub const DPAD_LEFT: u32 = 0x0004;
    pub const DPAD_RIGHT: u32 = 0x0008;
    pub const START: u32 = 0x0010;
    pub const BACK: u32 = 0x0020;
    pub const LEFT_THUMB: u32 = 0x0040;
    pub const RIGHT_THUMB: u32 = 0x0080;
    pub const LEFT_SHOULDER: u32 = 0x0100;
    pub const RIGHT_SHOULDER: u32 = 0x0200;
    /// Not in the public XInput headers; exposed by `XInputGetStateEx`.
    pub const GUIDE: u32 = 0x0400;
    pub const A: u32 = 0x1000;
    pub const B: u32 = 0x2000;
    pub const X: u32 = 0x4000;
    pub const Y: u32 = 0x8000;

    // Extended controls (bits 16+): present on richer pads, absent from XInput.
    pub const LEFT_PADDLE: u32 = 1 << 16;
    pub const RIGHT_PADDLE: u32 = 1 << 17;
    pub const LEFT_PADDLE2: u32 = 1 << 18;
    pub const RIGHT_PADDLE2: u32 = 1 << 19;
    /// Touchpad click (the whole pad presses), distinct from a finger touching it.
    pub const TOUCHPAD_CLICK: u32 = 1 << 20;
    /// DualShock Share / Xbox Series Share / Switch Capture.
    pub const SHARE: u32 = 1 << 21;
    /// Vendor-extra with no cross-pad meaning (Switch C / DualSense mic).
    pub const MISC1: u32 = 1 << 22;
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

/// Inertial measurement, in **canonical physical fixed-point** units so any pad
/// family can be driven from one representation: **gyro in `dps × 16`** (±2048 °/s)
/// and **accel in `g × 4096`** (±8 g). The client converts its sensor reading once;
/// the server converts to each family's native format. (These scales match a
/// DualSense's native sensitivities, so the Sony path is a near-passthrough.)
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Imu {
    pub gyro_pitch: i16,
    pub gyro_yaw: i16,
    pub gyro_roll: i16,
    pub accel_x: i16,
    pub accel_y: i16,
    pub accel_z: i16,
    /// The pad's own sensor clock; Sony reports carry it verbatim.
    pub sensor_timestamp: u32,
}

/// One touchpad contact. `x`/`y` are the pad's native range (Sony: 0–1919 by
/// 0–1079); the codec places them on the wire byte-for-byte.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Finger {
    pub active: bool,
    pub x: u16,
    pub y: u16,
    pub id: u8,
}

/// Both touchpad contacts of a two-finger pad.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Touchpad {
    pub finger0: Finger,
    pub finger1: Finger,
}

/// Battery and headset state a pad reports back for its own status LEDs.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Battery {
    pub level: u8,
    pub charging: bool,
    pub full: bool,
    pub mic_muted: bool,
    pub headphones: bool,
}

/// One pad's state: a controller-agnostic superset that feeds the HIDMaestro
/// codec (`sunburst_input::pad`), which packs it into whatever profile the pad
/// emulates. The core fields also map onto a ViGEm X360 report — that mapping is
/// the fallback when the native driver is unavailable, not the primary target.
///
/// The rich sections are optional because a field exists only for a pad that has
/// it: an Xbox pad carries none of them, a DualSense all three. They ride the
/// wire behind a presence mask (see [`presence`]), so the common case stays
/// compact.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct GamepadState {
    pub pad_index: u8,
    pub buttons: u32,
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
    pub lt: u8,
    pub rt: u8,
    pub imu: Option<Imu>,
    pub touchpad: Option<Touchpad>,
    pub battery: Option<Battery>,
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

/// Write a finger into a 5-byte slot: `id | lifted-bit`, then `x`, then `y`.
/// Bit 7 of byte 0 is set when the finger is *not* touching — the same convention
/// the codec's `touchpad-finger` op uses, so the server maps it across untouched.
fn encode_finger(f: &Finger, out: &mut [u8]) {
    out[0] = (f.id & 0x7F) | if f.active { 0x00 } else { 0x80 };
    out[1..3].copy_from_slice(&f.x.to_le_bytes());
    out[3..5].copy_from_slice(&f.y.to_le_bytes());
}

/// Read a finger from a 5-byte slot. Caller guarantees the length.
fn decode_finger(b: &[u8]) -> Finger {
    Finger {
        active: b[0] & 0x80 == 0,
        id: b[0] & 0x7F,
        x: u16::from_le_bytes([b[1], b[2]]),
        y: u16::from_le_bytes([b[3], b[4]]),
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
                let mut mask = 0u8;
                if g.imu.is_some() {
                    mask |= presence::IMU;
                }
                if g.touchpad.is_some() {
                    mask |= presence::TOUCHPAD;
                }
                if g.battery.is_some() {
                    mask |= presence::BATTERY;
                }
                body[0] = g.pad_index;
                body[1] = mask;
                body[2..6].copy_from_slice(&g.buttons.to_le_bytes());
                body[6..8].copy_from_slice(&g.lx.to_le_bytes());
                body[8..10].copy_from_slice(&g.ly.to_le_bytes());
                body[10..12].copy_from_slice(&g.rx.to_le_bytes());
                body[12..14].copy_from_slice(&g.ry.to_le_bytes());
                body[14] = g.lt;
                body[15] = g.rt;

                // The top-of-function guard proved `body` holds at least
                // `MAX_INPUT_BODY - INPUT_PREFIX` bytes, so every section below
                // fits without a further check.
                let mut at = GAMEPAD_CORE;
                if let Some(imu) = g.imu {
                    body[at..at + 2].copy_from_slice(&imu.gyro_pitch.to_le_bytes());
                    body[at + 2..at + 4].copy_from_slice(&imu.gyro_yaw.to_le_bytes());
                    body[at + 4..at + 6].copy_from_slice(&imu.gyro_roll.to_le_bytes());
                    body[at + 6..at + 8].copy_from_slice(&imu.accel_x.to_le_bytes());
                    body[at + 8..at + 10].copy_from_slice(&imu.accel_y.to_le_bytes());
                    body[at + 10..at + 12].copy_from_slice(&imu.accel_z.to_le_bytes());
                    body[at + 12..at + 16].copy_from_slice(&imu.sensor_timestamp.to_le_bytes());
                    at += GAMEPAD_IMU;
                }
                if let Some(tp) = g.touchpad {
                    encode_finger(&tp.finger0, &mut body[at..at + 5]);
                    encode_finger(&tp.finger1, &mut body[at + 5..at + 10]);
                    at += GAMEPAD_TOUCHPAD;
                }
                if let Some(bat) = g.battery {
                    let mut flags = 0u8;
                    if bat.charging {
                        flags |= battery_flags::CHARGING;
                    }
                    if bat.full {
                        flags |= battery_flags::FULL;
                    }
                    if bat.mic_muted {
                        flags |= battery_flags::MIC_MUTED;
                    }
                    if bat.headphones {
                        flags |= battery_flags::HEADPHONES;
                    }
                    body[at] = bat.level;
                    body[at + 1] = flags;
                    at += GAMEPAD_BATTERY;
                }
                at
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
                if body.len() < GAMEPAD_CORE {
                    return None;
                }
                let pad_index = body[0];
                if pad_index >= MAX_PADS {
                    // Refused at the boundary rather than trusted and used to
                    // index a ViGEm target array further in.
                    return None;
                }
                let mask = body[1];
                let buttons = u32::from_le_bytes([body[2], body[3], body[4], body[5]]);

                let mut at = GAMEPAD_CORE;
                let imu = if mask & presence::IMU != 0 {
                    if body.len() < at + GAMEPAD_IMU {
                        return None;
                    }
                    let s = &body[at..];
                    let imu = Imu {
                        gyro_pitch: i16::from_le_bytes([s[0], s[1]]),
                        gyro_yaw: i16::from_le_bytes([s[2], s[3]]),
                        gyro_roll: i16::from_le_bytes([s[4], s[5]]),
                        accel_x: i16::from_le_bytes([s[6], s[7]]),
                        accel_y: i16::from_le_bytes([s[8], s[9]]),
                        accel_z: i16::from_le_bytes([s[10], s[11]]),
                        sensor_timestamp: u32::from_le_bytes([s[12], s[13], s[14], s[15]]),
                    };
                    at += GAMEPAD_IMU;
                    Some(imu)
                } else {
                    None
                };
                let touchpad = if mask & presence::TOUCHPAD != 0 {
                    if body.len() < at + GAMEPAD_TOUCHPAD {
                        return None;
                    }
                    let tp = Touchpad {
                        finger0: decode_finger(&body[at..at + 5]),
                        finger1: decode_finger(&body[at + 5..at + 10]),
                    };
                    at += GAMEPAD_TOUCHPAD;
                    Some(tp)
                } else {
                    None
                };
                let battery = if mask & presence::BATTERY != 0 {
                    if body.len() < at + GAMEPAD_BATTERY {
                        return None;
                    }
                    let flags = body[at + 1];
                    Some(Battery {
                        level: body[at],
                        charging: flags & battery_flags::CHARGING != 0,
                        full: flags & battery_flags::FULL != 0,
                        mic_muted: flags & battery_flags::MIC_MUTED != 0,
                        headphones: flags & battery_flags::HEADPHONES != 0,
                    })
                } else {
                    None
                };

                InputEvent::Gamepad(GamepadState {
                    pad_index,
                    buttons,
                    lx: lei16(6),
                    ly: lei16(8),
                    rx: lei16(10),
                    ry: lei16(12),
                    lt: body[14],
                    rt: body[15],
                    imu,
                    touchpad,
                    battery,
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

    /// A DualSense-shaped pad with every rich section populated — the largest
    /// gamepad, and the one that exercises IMU, touchpad and battery at once.
    fn rich_pad() -> GamepadState {
        GamepadState {
            pad_index: 1,
            buttons: buttons::A | buttons::SHARE | buttons::TOUCHPAD_CLICK,
            lx: 1234,
            ly: -5678,
            rx: -1,
            ry: 32767,
            lt: 40,
            rt: 200,
            imu: Some(Imu {
                gyro_pitch: 1000,
                gyro_yaw: -2000,
                gyro_roll: 300,
                accel_x: 4096,
                accel_y: -8192,
                accel_z: 512,
                sensor_timestamp: 0xDEAD_BEEF,
            }),
            touchpad: Some(Touchpad {
                finger0: Finger {
                    active: true,
                    x: 960,
                    y: 540,
                    id: 3,
                },
                finger1: Finger {
                    active: false,
                    x: 100,
                    y: 200,
                    id: 7,
                },
            }),
            battery: Some(Battery {
                level: 8,
                charging: true,
                full: false,
                mic_muted: true,
                headphones: true,
            }),
        }
    }

    #[test]
    fn every_event_round_trips() {
        round_trip(InputEvent::Gamepad(GamepadState {
            pad_index: 3,
            buttons: buttons::A | buttons::DPAD_LEFT | buttons::GUIDE | buttons::LEFT_PADDLE2,
            lx: -32768,
            ly: 32767,
            rx: -1,
            ry: 1,
            lt: 0,
            rt: 255,
            ..Default::default()
        }));
        round_trip(InputEvent::Gamepad(rich_pad()));
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
        // A core-only pad (no rich sections): presence mask is zero and the body
        // is exactly GAMEPAD_CORE bytes.
        let mut buf = [0u8; MAX_INPUT_BODY];
        let n = InputPacket {
            input_seq: 1,
            event: InputEvent::Gamepad(GamepadState {
                pad_index: 2,
                buttons: 0x00AB_CDEF,
                lx: 0x0102,
                ly: 0x0304,
                rx: 0x0506,
                ry: 0x0708,
                lt: 0x11,
                rt: 0x22,
                ..Default::default()
            }),
        }
        .encode(&mut buf)
        .unwrap();

        assert_eq!(n, INPUT_PREFIX + GAMEPAD_CORE);
        assert_eq!(&buf[..5], &[0x01, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(
            &buf[5..n],
            &[
                0x02, // pad_index
                0x00, // presence: no sections
                0xEF, 0xCD, 0xAB, 0x00, // buttons u32
                0x02, 0x01, 0x04, 0x03, 0x06, 0x05, 0x08, 0x07, // sticks
                0x11, 0x22, // triggers
            ]
        );
    }

    #[test]
    fn a_rich_pad_encodes_its_sections_after_the_core() {
        let mut buf = [0u8; MAX_INPUT_BODY];
        let n = InputPacket {
            input_seq: 0,
            event: InputEvent::Gamepad(rich_pad()),
        }
        .encode(&mut buf)
        .unwrap();

        assert_eq!(n, MAX_INPUT_BODY);
        let body = &buf[5..n];
        // Presence mask names all three sections.
        assert_eq!(
            body[1],
            presence::IMU | presence::TOUCHPAD | presence::BATTERY
        );
        // Finger 0 is touching (bit 7 clear) with id 3; finger 1 lifted (bit 7 set).
        let tp = GAMEPAD_CORE + GAMEPAD_IMU;
        assert_eq!(body[tp], 0x03);
        assert_eq!(body[tp + 5] & 0x80, 0x80);
        // Battery flags: charging + mic + headphones, not full.
        let bat = GAMEPAD_CORE + GAMEPAD_IMU + GAMEPAD_TOUCHPAD;
        assert_eq!(
            body[bat + 1],
            battery_flags::CHARGING | battery_flags::MIC_MUTED | battery_flags::HEADPHONES
        );
    }

    #[test]
    fn every_presence_combination_round_trips() {
        let base = rich_pad();
        for imu in [None, base.imu] {
            for touchpad in [None, base.touchpad] {
                for battery in [None, base.battery] {
                    round_trip(InputEvent::Gamepad(GamepadState {
                        imu,
                        touchpad,
                        battery,
                        ..base
                    }));
                }
            }
        }
    }

    #[test]
    fn a_truncated_rich_pad_is_refused_not_decoded_short() {
        // A packet that claims sections but is cut off inside one must be refused,
        // never decoded as if the missing bytes were zero.
        let mut buf = [0u8; MAX_INPUT_BODY];
        let n = InputPacket {
            input_seq: 1,
            event: InputEvent::Gamepad(rich_pad()),
        }
        .encode(&mut buf)
        .unwrap();
        // Everything short of the full body is refused; the full body decodes.
        for len in (5 + GAMEPAD_CORE)..n {
            assert_eq!(
                InputPacket::decode(&buf[..len]),
                None,
                "a rich pad truncated to {len} bytes must be refused"
            );
        }
        assert!(InputPacket::decode(&buf[..n]).is_some());
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
        // and encode would start refusing at runtime instead. The fully-populated
        // gamepad is the largest payload.
        let mut buf = [0u8; MAX_INPUT_BODY];
        let n = InputPacket {
            input_seq: 0,
            event: InputEvent::Gamepad(rich_pad()),
        }
        .encode(&mut buf)
        .unwrap();
        assert_eq!(
            n, MAX_INPUT_BODY,
            "a rich gamepad should be the largest payload"
        );
    }
}

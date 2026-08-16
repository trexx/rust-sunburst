// SPDX-License-Identifier: GPL-2.0-or-later

//! A client that pairs and sends input, without a TV or an Android build.
//!
//! Kept rather than throwaway. It is the only end-to-end exercise of the
//! transport that runs on the development machine, it is what CI runs, and
//! Phase 4 will point it at the video path.
//!
//! ```text
//! fakeclient pair   --server 127.0.0.1:47811
//! fakeclient apps   --server 127.0.0.1:47811
//! fakeclient input  --server 127.0.0.1:47811 [--script gamepad-sweep]
//! ```
//!
//! Pairing prints the PIN it generated. Type that into the web UI — it is never
//! transmitted, and both ends derive the same secret from it independently.

use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use sunburst_core::proto::input::buttons;
use sunburst_core::proto::pairing::{NONCE_LEN, PIN_DIGITS, confirm_tag, derive_secret};
use sunburst_core::proto::{
    ClientControl, GamepadState, Hello, InputEvent, InputPacket, MouseButton, MouseMotion,
    PairRequest, ServerControl, SessionKey,
};
use sunburst_net::ClientEndpoint;

const DEFAULT_SERVER: &str = "127.0.0.1:47811";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let server = match option(&refs, "--server") {
        Some(raw) => match raw.parse::<SocketAddr>() {
            Ok(addr) => addr,
            Err(e) => {
                eprintln!("fakeclient: bad --server {raw}: {e}");
                return ExitCode::from(2);
            }
        },
        None => DEFAULT_SERVER.parse().expect("literal"),
    };
    let secrets = option(&refs, "--state")
        .map(PathBuf::from)
        .unwrap_or_else(default_state_path);

    let result = match refs.first().copied() {
        Some("pair") => pair(server, &secrets),
        Some("apps") => apps(server, &secrets),
        Some("input") => input(server, &secrets, option(&refs, "--script")),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fakeclient: {e}");
            ExitCode::FAILURE
        }
    }
}

const USAGE: &str = "\
usage:
  fakeclient pair  [--server host:port] [--state path]
  fakeclient apps  [--server host:port] [--state path]
  fakeclient input [--server host:port] [--state path] [--script name]

scripts: gamepad-sweep (default), keyboard, mouse

Pairing prints a PIN to type into the web UI. The PIN is never transmitted.";

fn option<'a>(args: &[&'a str], name: &str) -> Option<&'a str> {
    let at = args.iter().position(|a| *a == name)?;
    args.get(at + 1).copied()
}

fn default_state_path() -> PathBuf {
    std::env::var("SUNBURST_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("sunburst-dev"))
        .join("fakeclient.key")
}

/// The stored pairing: the secret, and where the input sequence got to.
///
/// The sequence has to persist. `input_seq` must strictly increase or the
/// server's replay window refuses the packet, and until per-session keys exist
/// (see below) the window is not reset between runs — so a second
/// `fakeclient input` starting from 1 again would silently do nothing.
struct State {
    secret: [u8; 32],
    next_input_seq: u32,
}

fn load_state(path: &PathBuf) -> Result<State, String> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        format!(
            "no pairing at {}: {e}. Run `fakeclient pair` first",
            path.display()
        )
    })?;
    let mut lines = text.lines();
    let raw = lines.next().unwrap_or("").trim();
    if raw.len() != 64 {
        return Err(format!("{} does not hold a 32-byte key", path.display()));
    }
    let mut secret = [0u8; 32];
    for (i, slot) in secret.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&raw[i * 2..i * 2 + 2], 16)
            .map_err(|_| format!("{} is not hex", path.display()))?;
    }
    let next_input_seq = lines
        .next()
        .and_then(|l| l.trim().parse().ok())
        .unwrap_or(1);
    Ok(State {
        secret,
        next_input_seq,
    })
}

fn save_state(path: &PathBuf, state: &State) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let hex: String = state.secret.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(path, format!("{hex}\n{}\n", state.next_input_seq)).map_err(|e| e.to_string())
}

/// Eight digits, uniformly. Rejection sampling for the same reason the server
/// does it: biasing the one secret an attacker may grind is the wrong place to
/// be approximate.
fn generate_pin() -> Result<String, String> {
    const LIMIT: u32 = 100_000_000;
    const CEILING: u32 = u32::MAX - (u32::MAX % LIMIT);
    loop {
        let mut bytes = [0u8; 4];
        getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
        let value = u32::from_le_bytes(bytes);
        if value < CEILING {
            return Ok(format!("{:0width$}", value % LIMIT, width = PIN_DIGITS));
        }
    }
}

fn generate_nonce() -> Result<[u8; NONCE_LEN], String> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|e| e.to_string())?;
    Ok(nonce)
}

/// Wait for a specific server message, ticking so retransmits go out.
fn await_message(
    client: &mut ClientEndpoint,
    what: &str,
    timeout: Duration,
) -> Result<ServerControl, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(message) = client.recv_control().map_err(|e| e.to_string())? {
            return Ok(message);
        }
        client.tick().map_err(|e| e.to_string())?;
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for {what}"));
        }
    }
}

fn pair(server: SocketAddr, state: &PathBuf) -> Result<(), String> {
    let mut client = ClientEndpoint::connect(server, None).map_err(|e| e.to_string())?;

    // Generated here and displayed, the way a TV would. It never goes on the
    // wire — the server derives from whatever the user types instead.
    let pin = generate_pin()?;
    let client_nonce = generate_nonce()?;

    client
        .send_control(&ClientControl::PairRequest(PairRequest {
            name: "fakeclient".into(),
            model: "development".into(),
            abi: std::env::consts::ARCH.into(),
            quirks: Default::default(),
            client_nonce,
        }))
        .map_err(|e| e.to_string())?;

    let (request_id, server_nonce) =
        match await_message(&mut client, "the pair challenge", Duration::from_secs(5))? {
            ServerControl::PairChallenge {
                request_id,
                server_nonce,
            } => (request_id, server_nonce),
            other => {
                return Err(format!(
                    "expected a challenge, got {other:?}. Is pairing armed in the web UI?"
                ));
            }
        };

    let secret = derive_secret(&pin, &client_nonce, &server_nonce);
    client
        .send_control(&ClientControl::PairConfirm {
            request_id,
            tag: confirm_tag(&secret),
        })
        .map_err(|e| e.to_string())?;

    println!();
    println!("    PIN: {pin}");
    println!();
    println!("Type that into the web UI to finish pairing.");
    println!("Secret stored at {}", state.display());
    save_state(
        state,
        &State {
            secret,
            next_input_seq: 1,
        },
    )?;

    // Keep answering for a moment so the confirmation is acknowledged rather
    // than retransmitted at a client that has already exited.
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        let _ = client.recv_control();
        client.tick().map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn hello(client_nonce: [u8; NONCE_LEN]) -> ClientControl {
    ClientControl::Hello(Hello {
        // A hint only. The MAC is what identifies this client.
        client_id: 1,
        name: "fakeclient".into(),
        abi: std::env::consts::ARCH.into(),
        width: 3840,
        height: 2160,
        refresh_mhz: 60_000,
        client_nonce,
        clock_offset_ns: 0,
    })
}

fn apps(server: SocketAddr, state: &PathBuf) -> Result<(), String> {
    let stored = load_state(state)?;
    let mut client = ClientEndpoint::connect(server, Some(SessionKey::from_bytes(stored.secret)))
        .map_err(|e| e.to_string())?;

    client
        .send_control(&hello(generate_nonce()?))
        .map_err(|e| e.to_string())?;
    client
        .send_control(&ClientControl::ListApps)
        .map_err(|e| e.to_string())?;

    match await_message(&mut client, "the app list", Duration::from_secs(5))? {
        ServerControl::AppList(apps) if apps.is_empty() => {
            println!("(no applications configured)");
        }
        ServerControl::AppList(apps) => {
            for app in apps {
                println!("{:>4}  {}", app.id, app.name);
            }
        }
        other => return Err(format!("expected an app list, got {other:?}")),
    }
    Ok(())
}

fn input(server: SocketAddr, state: &PathBuf, script: Option<&str>) -> Result<(), String> {
    let mut stored = load_state(state)?;
    let mut client = ClientEndpoint::connect(server, Some(SessionKey::from_bytes(stored.secret)))
        .map_err(|e| e.to_string())?;
    client
        .send_control(&hello(generate_nonce()?))
        .map_err(|e| e.to_string())?;

    let events = match script.unwrap_or("gamepad-sweep") {
        "gamepad-sweep" => gamepad_sweep(),
        "keyboard" => keyboard(),
        "mouse" => mouse(),
        other => return Err(format!("unknown script {other}")),
    };

    // Continues from where the last run stopped. Restarting at 1 would be
    // refused by the replay window, and the client cannot see that happen —
    // input is fire-and-forget — so it would look like it worked.
    for event in &events {
        client
            .send_input(&InputPacket {
                input_seq: stored.next_input_seq,
                event: *event,
            })
            .map_err(|e| e.to_string())?;
        stored.next_input_seq += 1;
        std::io::stdout().flush().ok();
        std::thread::sleep(Duration::from_millis(16));
    }

    save_state(state, &stored)?;
    println!(
        "sent {} events (input_seq now {})",
        events.len(),
        stored.next_input_seq
    );
    Ok(())
}

fn gamepad_sweep() -> Vec<InputEvent> {
    let mut events = Vec::new();
    // Each face button, pressed and released.
    for button in [buttons::A, buttons::B, buttons::X, buttons::Y] {
        for pressed in [button, 0] {
            events.push(InputEvent::Gamepad(GamepadState {
                pad_index: 0,
                buttons: pressed,
                ..Default::default()
            }));
        }
    }
    // A full stick sweep, so a sign or endianness error on an axis is visible
    // rather than plausible. Deliberately includes both extremes: the first
    // version of this negated the value for the Y axis and panicked at
    // i16::MIN, which is the exact asymmetry an axis handler gets wrong too.
    for step in 0..32 {
        let value = (step * 2048 - 32768).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        events.push(InputEvent::Gamepad(GamepadState {
            pad_index: 0,
            lx: value,
            ly: value.saturating_neg(),
            ..Default::default()
        }));
    }
    events
}

fn keyboard() -> Vec<InputEvent> {
    // "HELLO" as virtual-key codes, each down then up.
    let mut events = Vec::new();
    for vk in [0x48u16, 0x45, 0x4C, 0x4C, 0x4F] {
        events.push(InputEvent::KeyDown { vk, modifiers: 0 });
        events.push(InputEvent::KeyUp { vk, modifiers: 0 });
    }
    events
}

fn mouse() -> Vec<InputEvent> {
    let mut events = Vec::new();
    for step in 0..20 {
        events.push(InputEvent::MouseMove(MouseMotion::Relative {
            dx: 10,
            dy: if step % 2 == 0 { 5 } else { -5 },
        }));
    }
    events.push(InputEvent::MouseMove(MouseMotion::Absolute {
        x: 32_768,
        y: 32_768,
    }));
    events.push(InputEvent::MouseButton {
        button: MouseButton::Left,
        down: true,
    });
    events.push(InputEvent::MouseButton {
        button: MouseButton::Left,
        down: false,
    });
    events.push(InputEvent::MouseWheel {
        delta: 120,
        horizontal: false,
    });
    events.push(InputEvent::MouseWheel {
        delta: -120,
        horizontal: true,
    });
    events
}

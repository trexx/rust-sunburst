# Sunburst

Low-latency 4K HDR game streaming from a Windows PC to Android TV, in Rust. Sunburst is a streaming server for Windows with a dedicated Android TV client, written to replace Sunshine/Moonlight in a two-device household. It targets a fixed set of hardware and is not a general-purpose product.

## Overview

The primary workload is 4K60 HDR10 from Steam Big Picture Mode on a LAN.

### Hardware targets

Server: Windows 10 1903+ or Windows 11, an RTX 4070 (one NVENC), and a driver reporting NVENC API 13.1 or later. The Windows binary is compiled for `x86-64-v3` and will not run on a CPU without AVX2.

| Client | Codec | ABI |
|---|---|---|
| NVIDIA Shield TV | HEVC Main10 | arm64-v8a |
| Homatics Box R 4K Plus | AV1 Main10 | armeabi-v7a |

The codec is negotiated once per connection. Measured encoder, decoder and network results for this hardware are recorded in [HARDWARE_TESTING.md](HARDWARE_TESTING.md).

### How it works

- **Capture.** The server captures the whole monitor, not a window, using Windows Graphics Capture (Windows 11) or Desktop Duplication (Windows 10), each falling back to the other. NvFBC is available as an opt-in backend only.
- **Encode.** A GPU shader converts the desktop (scRGB on an HDR desktop) to 10-bit P010 for NVENC HEVC or AV1, using ultra-low-latency tuning. H.264 is an opt-in 8-bit SDR codec. Slices (HEVC) or tiles (AV1) are sent as soon as each is encoded, so encoding overlaps transmission.
- **Transport.** A bespoke protocol on a single UDP port ([PROTOCOL.md](PROTOCOL.md)). Video and audio are unencrypted and have no FEC; losses are handled with NACK and reference-frame invalidation. Input and control packets are always authenticated with keys derived from pairing. Sunburst is LAN-only.
- **Client.** An Android TV app: Kotlin owns the activities (app grid, stream, pairing, settings) and a Rust library loaded over JNI does the networking and decoding through MediaCodec. The mouse cursor is drawn on the client from shape data the server sends, and its movement is predicted locally.
- **Audio.** WASAPI loopback capture encoded with Opus. The microphone of an Xbox headset can be sent back to the server.
- **Input.** Keyboard and mouse are injected with `SendInput` (scancodes). Gamepads appear on the server as HIDMaestro virtual devices. The client can drive an Xbox Wireless Adapter directly, through vendored xow/xone driver code.
- **Management.** The server hosts a web UI and HTTP API for pairing, clients, sessions, the app catalogue and settings.

The frame path (capture through presentation) avoids allocation, locks, logging and async runtimes once warmed up, and records per-stage timings into a lock-free instrumentation ring. The reasoning behind these and the other design decisions, plus the current latency budget, is in [CLAUDE.md](CLAUDE.md). Phase-by-phase progress is in [ROADMAP.md](ROADMAP.md).

### Workspace layout

| Path | Contents |
|---|---|
| `sunburst-core/` | Protocol types, packets, timestamps, instrumentation; no I/O |
| `sunburst-capture/` | `Capture` trait and the DDA, WGC and NvFBC backends (Windows) |
| `sunburst-encode/` | NVENC FFI: HEVC and AV1 (10-bit), H.264 (8-bit SDR); vendored CUDA kernels (Windows) |
| `sunburst-audio/` | WASAPI loopback and Opus |
| `sunburst-input/` | Virtual pads, `SendInput`, desktop re-attach |
| `sunburst-net/` | UDP transport, pacing, NACK, rate control |
| `sunburst-server/` | Server binary and orchestration (Windows) |
| `sunburst-web/` | Management API: clients, pairing, sessions, configuration, apps |
| `sunburst-gip-bridge/` | Vendored xow/xone C++ and libusb behind a C FFI, for the Xbox Wireless Adapter on Android |
| `sunburst-android/` | Client library (`cdylib`) and JNI shim |
| `android/` | Gradle project for the Android TV app |
| `web/` | Management UI (Vite, React, TypeScript) |
| `tools/` | `fakeclient` (a client for testing without a TV), `probe-windows` (hardware harnesses), `check-phy.sh` |
| `third-party/hidmaestro/` | Vendored HIDMaestro controller profiles and driver INFs |
| `scripts/fetch-firmware.sh` | Fetches the Xbox Wireless Adapter firmware at build time |

## Usage

### Server

1. Get `sunburst-server.exe` and the built web UI, either from the `sunburst-windows` CI artifact (see [Releases](#releases)) or by building them (see [Development](#development)). Keep the `web/` directory next to the executable.
2. Run `sunburst-server.exe` in the logged-in user's session. There is no Windows service: capture and input injection need the interactive session, so with nobody logged in there is no server and no web UI. On start it prints the configuration directory, the web UI URL, the UDP stream address and the API token.
3. Open the web UI (by default `http://127.0.0.1:47810/`). To let TVs connect, set the bind address under *Web interface* to a LAN address. The UDP stream port binds to the same address, and a LAN bind requires a token.
4. Optionally enable autostart from the *Start-up* panel. It creates a scheduled task that runs at logon with highest privileges.
5. Add apps to the catalogue. Entries are typed in by hand; Steam Big Picture is an ordinary entry (`steam://open/bigpicture`).

Recommended host settings: turn off Enhanced Pointer Precision (or use the `disable_epp` input setting), run the audio render device at 48 kHz, run the desktop at a high refresh rate, and leave the desktop in HDR even for SDR clients.

### Android TV client

1. Sideload the `sunburst-android` APK from CI, or build it. It needs Android 11 (API 30) or later and contains both ABIs.
2. In the web UI, choose *Arm pairing (90s)*, then start pairing on the TV and enter the server's host (or `host:port`). The TV shows an eight-digit PIN; type it into the web UI. The PIN is never sent over the network.
3. Pick an app from the TV's grid to launch it on the server and start streaming.

## Configuration

Settings are stored in `config.json` and `clients.json` (plus uploaded box art in `art/`) in `%APPDATA%\sunburst`. Almost every capture, encoder, audio and input option is exposed in the web UI; changes take effect at the next session without a restart. Per-app overrides cover codec, bitrate and preset. The TV's settings screen can ask for a codec and a bitrate ceiling, which the server honours only within what the device supports, and sets local options such as jitter-buffer depth, cursor overlay and audio routing.

Some things are deliberately not configurable: there is no encryption toggle, presets are limited to P1–P4 within ultra-low-latency tuning, and NvFBC is never the automatic capture default.

Environment variables:

| Variable | Effect |
|---|---|
| `SUNBURST_CONFIG_DIR` | Overrides the configuration directory |
| `SUNBURST_DRIVER_INF` | Path to the extracted `hidmaestro.inf`; without it no virtual gamepad devices are created |
| `SUNBURST_NO_USO` | Disables UDP send offload on the server (bring-up switch) |
| `SUNBURST_FW_SHA256` | Overrides the firmware hash pinned in `scripts/fetch-firmware.sh` |
| `SUNBURST_DEBUG_KEYSTORE` | Keystore used to sign Android debug builds, so new builds install over old ones without losing the pairing |

## Development

Development happens on Linux and the server runs on Windows. The Rust toolchain and targets are pinned in `rust-toolchain.toml`.

```bash
cargo test --workspace                            # everything host-testable
cargo clippy --workspace --all-targets
cargo bench -p sunburst-core --bench instr

cargo xwin build --target x86_64-pc-windows-msvc  # Windows binaries, from Linux
cargo xwin clippy --workspace --all-targets --target x86_64-pc-windows-msvc

cargo build -p sunburst-android --target aarch64-linux-android
cargo build -p sunburst-android --target armv7-linux-androideabi
./gradlew -p android assembleDebug testDebugUnitTest

cd web && npm install && npm run build            # or npm run dev
```

Prerequisites for the Windows cross-build are `clang`, `lld` and `llvm`, plus `cargo install cargo-xwin --locked`. The Android builds need the NDK toolchain's `bin/` directory on `PATH`; `.cargo/config.toml` names the linker wrappers and sets per-target CPU flags, which a `RUSTFLAGS` environment variable would silently replace. The Gradle build compiles the Rust client library for both ABIs itself. To bundle the Xbox Wireless Adapter firmware into the APK, run `scripts/fetch-firmware.sh` first; the firmware is Microsoft's and is never committed.

Run `cargo xwin clippy` as well as plain clippy: the Windows-only crates compile to nothing on a Linux host. `tools/fakeclient` pairs, launches apps, sends input and receives streams without a TV, and is the end-to-end check that runs without hardware. Anything about real latency or device behaviour is verified on the target hardware and tracked in [HARDWARE_TESTING.md](HARDWARE_TESTING.md). Changes to a frame-path stage are expected to come with before/after p99 figures from the instrumentation ring.

CI (`.github/workflows/`):

- `ci.yml`, on pushes to `main`, pull requests and manual runs: Rust tests, clippy and fmt on Linux and Windows; the web UI build and type check; both Android ABIs, Android clippy, Gradle unit tests and the debug APK; and the packaged Windows binaries.
- `cuda-kernel.yml`, manual: compiles the NvFBC path's CUDA kernels to PTX for vendoring into `sunburst-encode/cuda/`.
- `driver.yml`, manual: extracts the HIDMaestro driver DLLs from a HIDMaestro release.

Dependencies are updated by Renovate (`.github/renovate.json`).

## Releases

There are no tagged releases. Builds are published as GitHub Actions artifacts from `ci.yml`:

- `sunburst-windows`: `sunburst-server.exe`, `fakeclient.exe` and the built web UI, on pushes to `main` and manual runs.
- `sunburst-android`: the debug-signed APK with both ABIs and the adapter firmware, on every run including pull requests.

## License

Sunburst is licensed under the GNU General Public License v2.0 or later; see [LICENSE](LICENSE). Vendored third-party code keeps its own provenance and licence notes in `sunburst-gip-bridge/vendor/UPSTREAM.md` and `third-party/hidmaestro/`.

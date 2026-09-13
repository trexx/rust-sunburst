# Sunburst — low-latency 4K HDR game streaming

Rust game streaming server for Windows, with a dedicated Android TV client.
Replaces Sunshine/Moonlight for a two-device household. Not a general-purpose product.

## Hardware targets (fixed — do not design for anything else)

**Server**
- Windows 10 1903+ and Windows 11
- RTX 4070 (AD104, Ada Lovelace, 8th-gen NVENC), 12 GB
- **1 NVENC + 1 NVDEC.** No Split Frame Encoding — it needs 2+ encoders and
  AD104 has one.
- Built against NVIDIA Video Codec SDK 13.1, and the startup check requires the
  driver to report **NVENC API ≥ 13.1** — which is what the code actually
  compares, so state the API version rather than a driver branch number. AV1
  encode arrived with Ada and needs only SDK 12.0+, so that floor is about the
  headers this build is written against, not about AV1.
- The encoder caps are **measured, not assumed** — see `HARDWARE_TESTING.md` §1.
  The load-bearing result: AV1 has parity with HEVC on reference invalidation and
  subframe readback.

**Clients**
| Device | SoC | Codec | ABI | Link |
|---|---|---|---|---|
| NVIDIA Shield TV | Tegra X1 | HEVC Main10 **only** (no AV1 block) | arm64-v8a | 1GbE wired |
| Homatics Box R 4K Plus | Amlogic S905X4 | AV1 Main10 (HEVC decoder buggy) | armeabi-v7a | verify PHY is 1GbE, not 100Mbit |

Codec is negotiated once at connect. Never both at once.
Homatics ships a 64-bit SoC with a 32-bit userspace — `armeabi-v7a` is required, not optional.

**Primary workload:** 4K60 HDR10, Steam Big Picture Mode.

## Architectural decisions (settled — do not relitigate)

- **Raw UDP, not QUIC.** RFC 9221 datagrams are congestion-controlled; QUIC's CC
  would fight our delay-gradient rate controller and its pacer optimises for RTT,
  not frame deadline. Also QUIC mandates TLS, and video here is unencrypted.
- **No video encryption. No FEC.** LAN-only. NACK + reference-frame invalidation
  beats Reed-Solomon on both latency and bandwidth at sub-ms RTT.
- **Input and control packets MUST be authenticated** (keyed BLAKE3 or SipHash +
  sequence number, shared secret from pairing). An unauthenticated UDP port that
  calls `SendInput` is a remote input-injection hole.
- **Capture the monitor, never a window.** Big Picture launches games in new
  windows with new swapchains; display capture rides through it invisibly.
- **Render the cursor client-side** from separately-delivered shape data. Removes
  the network round-trip from perceived pointer latency. Biggest single
  responsiveness win in the system.
- **Subframe readback is mandatory,** not an optimisation. With one NVENC and no
  SFE, encode time (5–10ms) is a fixed floor. Emitting slices (HEVC) / tiles (AV1)
  as they complete overlaps encode with transmit and is the only way to hide it.
- **Never enable AV1 UHQ mode.** It buys compression via pre-analysis — exactly
  the latency we're eliminating. Stay on `TUNING_INFO_ULTRA_LOW_LATENCY`, P1–P4.
- 4:2:2 and MV-HEVC are Blackwell additions this card does not have, and would be
  irrelevant if it did — Android decoders want 4:2:0. AV1 4:4:4 is absent too
  (HEVC 4:4:4 is present); also irrelevant, and noted only so its absence is
  never read as a finding.

## Hot path rules

Hot paths: capture, colour convert, encode, packetize, send, receive, jitter
buffer, decode, present, input inject.

1. **Zero allocation after warmup.** Preallocated pools. No `Vec::push` that can
   grow, no `String`, no `format!`, no `Box` in the frame path.
2. **No tokio.** Async is control-plane only (pairing, session setup, config).
   Frames and input never touch an async runtime.
3. **Dedicated OS threads** with MMCSS "Games" registration and
   `THREAD_PRIORITY_TIME_CRITICAL`.
4. **No locks in the frame path.** SPSC ring buffers between stages. If you think
   you need a `Mutex`, you need a different data structure.
5. **No logging in the frame path.** Push `(stage_id, frame_id, qpc_ticks)` into
   the lock-free instrumentation ring; a low-priority thread drains it.

### PR gate

Any change touching a hot path ships a before/after **p99** from the
instrumentation ring in the PR description. No numbers, no merge. This applies to
changes that look free — the point is to catch the ones that aren't.

## Latency budget (4K60 HDR, measured targets)

| Stage | Target | Notes |
|---|---|---|
| DWM composition | ~16.7ms | Only swapchain hooking may avoid this; NvFBC does not |
| Capture acquire | 0.5–2ms | |
| scRGB→P010 shader | 0.8–1.5ms | |
| NVENC HEVC P1 ULL | 5–9ms | Fixed floor; one NVENC, so no SFE |
| NVENC AV1 P1 ULL | 6–10ms | |
| Packetize + send | <0.5ms | USO offload |
| Wire @ 1GbE | 1–3ms | |
| Jitter buffer | 0–8ms | Adaptive |
| MediaCodec decode | 8–16ms | |
| Panel | 1–3 frames | Game Mode mandatory |

Honest glass-to-glass: **60–100ms**. Sub-40ms claims elsewhere measure
capture-to-wire, not what the eye sees. Do not chase them.

The DWM row is still an **assumption** — the largest line in the table, and never
measured directly. What *is* measured is that **NvFBC does not remove it**:
GPU-resident capture via `NvFBCToCuda` came in at 0.86–0.94× Desktop Duplication
in every controlled run (`HARDWARE_TESTING.md` §1), so the backend is struck and
swapchain hooking is the only remaining candidate. Pricing the row itself needs
the latency rig, not a throughput probe.

The two NVENC rows are **targets, not measurements** — they were written against
the Blackwell encoder this project originally assumed and have not been measured
on Ada. `HARDWARE_TESTING.md` §4 is where the real numbers land, and they replace
these when they arrive.

Bitrate: HEVC 100–150 Mbps, AV1 70–100 Mbps. Shield's decoder caps out before
1GbE does — treat ~150 Mbps as its practical ceiling.

## Crate layout

```
sunburst-core/     protocol types, packets, timestamps, instrumentation. no I/O.
sunburst-capture/  Capture trait + 5 backends
sunburst-encode/   NVENC FFI, HEVC + AV1
sunburst-audio/    WASAPI loopback + Opus
sunburst-input/    ViGEm + SendInput + session helper
sunburst-net/      UDP, pacing, NACK, rate control
sunburst-server/   orchestration; tokio lives here and only here
sunburst-web/      management API: clients, sessions, config, apps. Cross-platform.
sunburst-android/  cdylib + JNI shim
android/           Gradle project; Kotlin owns Activity + SurfaceView only
web/               Vite + React + TS management UI
tools/             development tools. Kept, unlike spikes/.
spikes/            Phase 0 throwaway. Deletable by design.
```

`sunburst-capture`, `-encode`, `-audio` and `-server` are `#![cfg(windows)]` at
the crate root, so they compile to nothing on a Linux host. `sunburst-core`,
`-net` and `-web` are cross-platform: the client needs the protocol types and the
receive half, and everything Windows-specific the web UI needs sits behind the
`Host` trait in `sunburst-web/src/host.rs`.

`sunburst-input` is the interesting case — deliberately **not** Windows-only at
the root. Its `keymap` module holds the decisions (scancode, extended flag,
modifier reconciliation, `MOUSEEVENTF` bits) as pure functions with real tests,
and only `inject` is gated. Those decisions are the part that is easy to get
wrong, so they are tested on a machine where `SendInput` does not exist.

That trait is not abstraction for its own sake — it is what lets the entire
management surface be tested on the Linux machine instead of the 4070 box.
`sunburst-server` implements it against Win32; `host::Fake` implements it for
tests.

## Process model

**There is no Windows service.** Capture and `SendInput` both require the
interactive session, so nothing useful can live in session 0. The server runs in
the logged-in session and autostarts through a scheduled task. That drops a
service, an installer and a session-0 IPC surface, and it makes launching a game
a plain `CreateProcess` rather than `WTSGetActiveConsoleSessionId` →
`CreateProcessAsUser`.

The consequence, stated rather than discovered: **with nobody logged in there is
no server and no web UI.** Capture could not work then either, so a service
would not have recovered anything.

The session helper in the traps below is still needed for the *desktop* problem —
re-attaching via `OpenInputDesktop`/`SetThreadDesktop` when the desktop switches
— just not for a cross-session one.

## Build and test

Development happens on Linux; the server runs on Windows. Both halves are always
checkable, and neither command lies about the other.

```bash
cargo test --workspace                            # everything host-testable
cargo clippy --workspace --all-targets            # SAFETY comments are enforced
cargo bench -p sunburst-core --bench instr        # record() must stay under 50ns

cargo xwin build --target x86_64-pc-windows-msvc  # real PE binaries, from Linux
cargo xwin clippy --workspace --all-targets --target x86_64-pc-windows-msvc

cargo build -p sunburst-android --target aarch64-linux-android
cargo build -p sunburst-android --target armv7-linux-androideabi
./gradlew -p android assembleDebug testDebugUnitTest

cd web && npm install && npm run build            # or `npm run dev`
```

**Run `cargo xwin clippy` as well as the plain one.** Clippy on a Linux host
skips every `#![cfg(windows)]` crate entirely, so the Windows code is unlinted
unless it is asked for by target — which is how four missing SAFETY comments sat
in the Phase 0 probe unnoticed.

Prerequisites:

```bash
sudo dnf install clang lld llvm     # clang-cl, lld-link, llvm-lib
cargo install cargo-xwin --locked   # fetches the MSVC CRT and Windows SDK
```

`llvm` is easy to miss: `cargo xwin` gets all the way through downloading the
Windows SDK and then fails in `cc-rs` looking for `llvm-lib`.

The Android targets need the NDK's toolchain `bin/` on `PATH` —
`.cargo/config.toml` names the linker wrappers and says which. The Gradle wrapper
fetches its own JDK.

Real NVENC and every hardware measurement happen on the 4070 box. The
cross-build produces genuine binaries but proves nothing about behaviour.

## Dependencies

The frame path stays bare: `sunburst-core` has `blake3`, `subtle` and `libc`,
and `serde` is deliberately kept out of it — where the web layer needs core's
types it mirrors them (`client::QuirksRecord`, `metrics::MetricsRecord`) rather
than deriving on the originals.

The control plane is allowed more, and `sunburst-web` is where it goes: `tokio`,
`hyper`, `hyper-util`, `http-body-util`, `serde`, `serde_json`, `getrandom`.
Roughly 25 crates. The line taken there is worth repeating, because it is not
"minimal" in the abstract: **routing is hand-rolled because an admin API is a
match statement; parsing is not hand-rolled, because the LAN-facing parser is the
last place to save five crates.**

The frontend runs to React and nothing else — no CSS, UI, state or router
libraries. Styling is plain CSS with custom properties in a single
`web/src/App.css`.

**What CI can and cannot tell you.** It compiles all three targets and runs the
host-testable tests. It cannot measure latency — no GPU, no hardware decoder, no
host, and the client ABIs are not the runner's. Do not add emulator benchmarks
to make it look like it can. Real latency work happens on real hardware, and
`HARDWARE_TESTING.md` tracks it.

**The noise floor for the PR gate is about 3%.** Two identical `sunburst-instr
selftest` runs on an idle machine differ by 0–3% at p99. A change smaller than
that is not distinguishable from scheduling noise in a single run — take more
runs or say the measurement was inconclusive, rather than reporting a 1%
improvement as if it were real.

FFI rule: wrap `unsafe` thin and early, at the crate boundary. Do not try to make
the D3D11/NVENC shim elegant — it's ~90% unsafe transcription from C++ samples.
Rust's value here is the protocol and state-machine code, not the GPU boundary.

## Known traps

**Input**
- Send **scancodes**, not virtual keys. `KEYEVENTF_SCANCODE` +
  `MapVirtualKeyW(vk, MAPVK_VK_TO_VSC_EX)`. Games reading DirectInput/raw input
  see scancodes only — a VK-based `SendInput` works on the desktop and does
  nothing in-game. Set `KEYEVENTF_EXTENDEDKEY` for arrows, right Ctrl/Alt,
  Ins/Del/Home/End/PgUp/PgDn, numpad Enter.
- `SendInput` targets the calling thread's **desktop**, and desktop attachment
  is per-thread. UAC and the lock screen switch the desktop *inside* the session,
  so a process that never went anywhere finds itself attached to the wrong one
  and its input silently goes nowhere. Re-attach via
  `OpenInputDesktop`/`SetThreadDesktop` from the same thread that injects.
  `OpenInputDesktop` returns a fresh handle every call, so compare by **name**
  (`GetUserObjectInformationW`, `UOI_NAME`) — two handles to one desktop are
  different values. There is no notification for this;
  `WTSRegisterSessionNotification` reports *session* changes, which this is not,
  so poll.
  (This trap once said the helper must live in the interactive session via
  `WTSGetActiveConsoleSessionId` → `CreateProcessAsUser`. That went away with the
  no-service decision — see *Process model*. The whole server is already in the
  interactive session; only the desktop problem survives, and it is a different
  and smaller one.)
- **Turn Enhanced Pointer Precision off on the server.** It applies an
  acceleration curve to injected relative deltas. Compensating was considered and
  declined: the curve is undocumented and varies with pointer speed, and being
  subtly wrong reads as "the mouse feels off", which is close to unattributable.
  One checkbox per install beats a guess that drifts.
- Steam Input grabs ViGEm pads and presents its own emulated device. Usually
  transparent; occasionally double-enumerates. Test this path early — it presents
  as "controller does nothing in one specific game".
- ViGEmBus is blocklisted by some kernel anti-cheats. `SendInput` sets
  `LLKHF_INJECTED`. Inherited support burden; nothing to be done.

**Capture**
- DDA and WGC both return black on DRM-protected content and both die on the
  secure desktop (UAC, lock screen). Detect via `OpenInputDesktop` failure and
  send a placeholder — never a frozen frame.
- `AccessLost` is recoverable and happens constantly (mode changes, fullscreen
  transitions, desktop switches). The `Capture` trait must survive rebuild at any
  moment.
- WGC requires `CreateFreeThreaded` frame pool. The non-free-threaded version
  dispatches on a UI thread and destroys latency.
- `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)` or coordinates are wrong
  on scaled displays.
- ShadowPlay / Instant Replay / OBS open their own session on our single physical
  NVENC. The driver time-shares it and per-frame encode times get jittery in a way
  that looks like our bug. Warn at startup if another session is detected.

**HDR**
- Windows 10 HDR is a **global display toggle**, not per-app like Win11. Toggle
  via `DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE` in `SetDisplayConfig` around the
  session, and restore on disconnect. Users will notice if you don't.
- Capture yields scRGB linear FP16. Shader: normalise by 80 nits → BT.2020
  primaries → PQ EOTF⁻¹ → 4:2:0 subsample. Get chroma siting right or UI text fringes.

**AV1 is not a flag on the HEVC path**
- OBUs, not NAL units. No Annex-B start codes. Packetize on OBU boundaries;
  respect `obu_has_size_field`.
- MediaCodec `csd-0` is an **av1C record**, not raw headers. Marker/version byte,
  `seq_profile`, `seq_level_idx`, `seq_tier`, bit-depth flags, then the sequence
  header OBU. Wrong av1C = decoder configures fine and silently outputs nothing.
- Tiles replace slices for subframe packetization. Start at 2×2 for 4K.
- AV1's reference model (8 slots, explicit signalling) differs enough from HEVC
  that reference invalidation needs a **separate** state machine, not a shared one.
- Query `NvEncGetEncodeCaps` with `NV_ENC_CODEC_AV1_GUID` for
  `SUPPORT_REF_PIC_INVALIDATION` and `SUPPORT_INTRA_REFRESH`. Do not assume
  parity. On this card both came back supported (`HARDWARE_TESTING.md` §1), so
  Phase 4 keeps reference invalidation on both codecs — but keep the query, since
  it is what makes the code correct on a machine that answers differently.

**Client**
- `SurfaceView`, never `TextureView` — TextureView costs a full frame of compositing.
- Pace presentation off `Choreographer` with `releaseOutputBuffer(index, timestampNanos)`.
  Immediate release produces microstutter that reads worse than the actual latency.
- Amlogic decoders are known to mishandle intra-refresh and reference invalidation.
  Before concluding the Homatics HEVC decoder is broken, retest with periodic IDR
  and no intra-refresh — the bug may be ours.
- Gate `PerformanceHintManager` (API 31) behind a version check. Real win on the
  Amlogic's small cores.
- Enumerate `MediaCodecList` at startup. Never assume a codec exists.

## Decoder quirks table

Keyed on `MediaCodecInfo.getName()` + `Build.MODEL`, sent to the server at handshake.
Exists from day one, not retrofitted.

```rust
struct DecoderQuirks {
    ref_invalidation: bool,
    intra_refresh: bool,
    slice_output: bool,
    needs_annexb_startcodes: bool,
    max_bitrate_hint: u32,
}
```

## Android build config

```
compileSdk 37   targetSdk 34   minSdk 30
abiFilters 'arm64-v8a', 'armeabi-v7a'
```

minSdk 30 is deliberate: `KEY_LOW_LATENCY` and `Surface.setFrameRate()` both
landed exactly there.

```toml
# .cargo/config.toml
[target.aarch64-linux-android]
linker = "aarch64-linux-android30-clang"
[target.armv7-linux-androideabi]
linker = "armv7a-linux-androideabi30-clang"
```

targetSdk 34 brings Android 14 rules: declare `foregroundServiceType`, and
runtime-registered receivers need explicit `RECEIVER_EXPORTED`/`RECEIVER_NOT_EXPORTED`.
If AGP rejects compileSdk 37, `android.suppressUnsupportedCompileSdk=37` unblocks.
Sideload-only; if this ever goes to Play, targetSdk 34 will be below the floor.

TV manifest: `android.software.leanback` required,
`android.hardware.touchscreen` required=false, `LEANBACK_LAUNCHER` filter, `android:banner`.

## Licensing

**Sunburst is GPL-2.0-or-later.** Not a preference — the Xbox Wireless Adapter
support (Phase 8) vendors the MT7612U radio driver from xow/xone, which is
GPL-2-or-later, and that makes the distributed client a derivative work. One
licence at the repo root rather than a per-directory split, so there is no
boundary to get wrong later. New files carry an SPDX header.

Microsoft's dongle firmware is **not** committed. `scripts/fetch-firmware.sh`
retrieves it at build time, following xone's own reasoning: xone declines to
redistribute it and so do we, which is what keeps this repo publishable.

Sunshine is GPL-3. Read it for protocol and Windows-integration understanding.
Combining with it is now *legally* possible — GPL-2-or-later can be distributed
as GPL-3 — so the reason to keep away is the engineering one, and it is the one
that mattered all along: Sunburst's value is its own protocol and state-machine
code, not a transcription of someone else's. Abstain by choice, not by licence.

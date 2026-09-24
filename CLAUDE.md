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
- **Three capture backends, and NvFBC is never the default.** WGC on Win11, DDA
  on Win10, each falling back to the other — they measure within half a
  millisecond of each other, so that choice is about compatibility and nothing
  else. NvFBC is opt-in only: it needs an undocumented private-data key a driver
  update can invalidate, and NVIDIA's last supported Windows 10 build for it is
  1803, below this project's 1903 floor. **It is kept for resilience alone** —
  DDA goes black on DRM-protected content and dies on the secure desktop. It is
  0.86–0.94× DDA on throughput and **1.2–1.5ms worse on latency**, so it is not
  kept for speed; that was the open question and it has been answered.
- **The `Capture` trait yields a backend-tagged frame** — `Frame::Texture` (a
  D3D11 texture, from DDA/WGC) or `Frame::Cuda` (a CUDA surface, from NvFBC).
  Each backend then takes its **shortest path** to NVENC: DDA/WGC → HLSL convert →
  NVENC DirectX input; **NvFBC stays CUDA-native** → a CUDA convert kernel → NVENC
  `CUDADEVICEPTR` input, never bounced through D3D11. This revises the original
  "the trait yields a D3D11 texture whatever produced it; NvFBC copies device-to-
  device via `cuGraphicsD3D11RegisterResource`": DDA/WGC and NvFBC are mutually
  exclusive at runtime, so unifying them onto one D3D11 path bought no sharing and
  only added a device-to-device copy on the opt-in resilience backend. The cost of
  the split, paid deliberately: the scRGB→P010 convert exists twice (an HLSL
  compute shader for the D3D11 backends, a P010 CUDA kernel for NvFBC) and the
  encoder registers two NVENC input types. (The NvFBC CUDA-native spine —
  convert kernel + NVENC-CUDA session — is now wired into
  `sunburst-server::pipeline` beside the built D3D11 path, and its convert
  kernels are real PTX vendored from `cuda-kernel.yml` — never hand-written;
  `sunburst-encode/tests/ptx_vendored.rs` checks the entry points. What is left
  is the box run, `HARDWARE_TESTING.md` §9.)
- **Render the cursor client-side** from separately-delivered shape data. Removes
  the network round-trip from perceived pointer latency. Biggest single
  responsiveness win in the system.
- **Subframe readback is mandatory,** not an optimisation. With one NVENC and no
  SFE, encode time (~9–11ms p99 at 4K on this card) is a fixed floor. Emitting slices (HEVC) / tiles (AV1)
  as they complete overlaps encode with transmit and is the only way to hide it.
- **Never enable AV1 UHQ mode.** It buys compression via pre-analysis — exactly
  the latency we're eliminating. Stay on `TUNING_INFO_ULTRA_LOW_LATENCY`, P1–P4.
- 4:2:2 and MV-HEVC are Blackwell additions this card does not have, and would be
  irrelevant if it did — Android decoders want 4:2:0. AV1 4:4:4 is absent too
  (HEVC 4:4:4 is present); also irrelevant, and noted only so its absence is
  never read as a finding.
- **H.264 is an opt-in, low-latency, 8-bit SDR codec — never an HDR path.** NVENC
  has no 10-bit H.264, so H.264 cannot carry HDR10; it exists because it encodes a
  touch faster than HEVC and is the lowest-latency hardware decode on cheap SoCs.
  `negotiate_codec` never auto-selects it over HEVC/AV1 (auto keeps HDR); it is
  chosen only by an explicit codec preference or as a client's sole offer. It
  reuses the HEVC packetizer (Annex-B NAL), the sequence-header path (SPS/PPS),
  and HEVC's reference-invalidation `Window`. Its one new piece is an **8-bit SDR
  pixel path**: a second convert — an scRGB→NV12 BT.709 HLSL shader and an
  `argb_to_nv12` CUDA kernel, both with an ACES HDR→SDR tonemap so H.264 works
  from an HDR desktop — feeding NVENC NV12 input instead of P010. HEVC/AV1 stay
  10-bit P010 throughout.

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

## Latency budget (4K60 HDR)

**Bold** figures are measured on env S; the rest are still estimates.

| Stage | Figure | Notes |
|---|---|---|
| DWM composition | **~half the refresh interval** | ~3.5ms at 144Hz, ~8.3ms at 60Hz (below); nothing escapes it |
| Capture acquire | **~0.4ms** | present→capture minus composition (below) |
| scRGB→P010 shader | **0.08–0.13ms p99** | was estimated at 0.8–1.5ms |
| NVENC HEVC P1 ULL | **9.2–11.3ms p99** | enc-unit, submit → each slice out. Fixed floor; one NVENC, so no SFE |
| NVENC AV1 P1 ULL | **10.6ms p99** | enc-unit; 2×2 tiles |
| Packetize | **14–21µs p99** | |
| Send | **4.6–9.3ms p99** | includes the pacer queue; p95 0.9–3.6ms; the tail varies 7.7–52ms run to run, so it is not quotable yet |
| Wire @ 1GbE | 1–3ms | |
| Jitter buffer | 0–8ms | Adaptive |
| MediaCodec decode | 8–16ms | |
| Panel | 1–3 frames | Game Mode mandatory |

Honest glass-to-glass: **60–100ms**. That figure was summed with composition at
16.7ms, so it is **pessimistic by roughly 9–13ms** on that line, while encode came
in at or just past the top of its estimate. It is not restated as a new total:
the client half (jitter, decode, panel) is still an estimate, and a corrected sum
of estimates is not a measurement. Sub-40ms
claims elsewhere measure capture-to-wire, not what the eye sees; do not chase
them.

The capture row survives its own measurement: subtracting half a refresh interval
from the ~4ms present→capture figure leaves about **0.4ms** for the capture
itself, at the bottom of the 0.5–2ms estimated for it.

**The composition row used to read ~16.7ms and it was wrong.** Measured
present→capture is ~4ms typical and ~7.5ms worst on a 144Hz display
(`HARDWARE_TESTING.md` §7). The old figure was the *worst case at 60Hz* recorded
as though it were the typical cost at any refresh rate.

**Composition costs about half the desktop's refresh interval.** So the single
cheapest latency win in the whole system is a display setting: **run the server's
desktop at a high refresh rate.** 144Hz costs ~3.5ms where 60Hz costs ~8.3ms,
which is ~5ms for free and more than several planned optimisations are worth.

**Nothing escapes composition.** With tearing enabled and ~15,000 presents/sec,
DDA, WGC and NvFBC all delivered 123–133 distinct frames/sec — the refresh rate.
That is measured, not assumed, and it is why swapchain hooking is no longer
carried as a latency win.

The server rows come from **single 5–30s runs** at 50–100 Mbps
(`HARDWARE_TESTING.md` §4 has the per-run table and commits). They replace the
Blackwell-era estimates this project started from (HEVC 5–9ms, AV1 6–10ms, shader
0.8–1.5ms, packetize+send <0.5ms). Sustained-run numbers replace them in turn.

Bitrate: HEVC 100–150 Mbps, AV1 70–100 Mbps. Shield's decoder caps out before
1GbE does — treat ~150 Mbps as its practical ceiling. **A device with no quirks
entry is capped at 50 Mbps** (`DecoderQuirks.max_bitrate_hint`'s default). To test
real 4K bitrates, pair with `fakeclient pair --max-bitrate-hint KBPS`, or seed the
device's quirks.

## Crate layout

```
sunburst-core/     protocol types, packets, timestamps, instrumentation. no I/O.
sunburst-capture/  Capture trait + 3 backends (DDA, WGC, NvFBC)
sunburst-encode/   NVENC FFI, HEVC + AV1 (10-bit) + H.264 (8-bit SDR)
sunburst-audio/    WASAPI loopback + Opus
sunburst-input/    HIDMaestro virtual pads + SendInput + desktop re-attach
sunburst-net/      UDP, pacing, NACK, rate control
sunburst-server/   orchestration; tokio lives here and only here
sunburst-web/      management API: clients, sessions, config, apps. Cross-platform.
sunburst-gip-bridge/ vendored xow/xone C++ (MT7612U radio + GIP + wired) + libusb
                   + shim, behind a C FFI Rust calls. Host = pure-Rust stub; the
                   real path is #[cfg(all(target_os="android", feature="vendored"))].
sunburst-android/  cdylib + JNI shim
android/           Gradle project; Kotlin owns Activity + SurfaceView, and
                   forwards the input and platform queries with no NDK equivalent
web/               Vite + React + TS management UI
tools/             development tools: fakeclient, probe-windows, check-phy.sh
```

**`tools/probe-windows` is the Phase 0 probe, kept.** Its NvFBC code (the keyed
`CreateEx`, the V2-not-V3 setup struct, the vtable slot order, the CUDA teardown
order — each wrong at least once before it was right) is promoted into
`sunburst-capture`; the probe stays because it still holds the hardware
harnesses nothing else has: NvFBC status/`NvFBC_Enable`, the NVML second-session
query, the present→capture latency harness (`HARDWARE_TESTING.md` §7) and the
direct-pad baseline (§8). `tools/check-phy.sh` is the §3 link-rate check.

`sunburst-capture`, `-encode` and `-server` are `#![cfg(windows)]` at the crate
root, so they compile to nothing on a Linux host. `sunburst-core`, `-net` and
`-web` are cross-platform: the client needs the protocol types and the receive
half, and everything Windows-specific the web UI needs sits behind the `Host`
trait in `sunburst-web/src/host.rs`.

`sunburst-input` and `sunburst-audio` are the interesting cases — deliberately
**not** Windows-only at the root. `sunburst-input`'s `keymap` module holds the
decisions (scancode, extended flag, modifier reconciliation, `MOUSEEVENTF` bits)
as pure functions with real tests, and only `inject` is gated.
`sunburst-audio`'s `codec` (a thin safe wrapper over `unsafe-libopus`, libopus
transpiled to pure Rust) and `pcm` (float→i16, downmix, frame regrouping) are
pure and host-tested, and the **client links the Opus decoder from the same
crate**; only the WASAPI `capture`/`device` modules are `#[cfg(windows)]`. Those
pure decisions are the part that is easy to get wrong, so they are tested on a
machine where WASAPI and `SendInput` do not exist.

**Audio wants the render endpoint at 48 kHz.** Loopback capture reads the
endpoint's shared mix format, which cannot be changed; the code converts
float32/16-bit and downmixes to stereo but does not resample, and warns if the
endpoint is not 48 kHz. One checkbox per install, like Enhanced Pointer
Precision. The capture device is selectable (`StreamConfig.audio_device`): the
default endpoint leaves the host audible, "Steam Streaming Speakers" silences it
by reusing Valve's signed sink rather than a driver of ours.

**The pad-headset mic reuses the same posture, inbound.** An Xbox headset's mic
(Phase 8) is forwarded to the server and rendered into a **consumed** virtual
microphone — a signed "Steam Streaming Microphone" endpoint chosen by name
(`StreamConfig.mic_device`, off by default), never a driver of ours and never
VB-CABLE. The mic travels at its native capture rate (a chat headset is 24 kHz
mono); the server's 48 kHz Opus decoder resamples on decode, so the no-resample
rule above is untouched. Client-side, `audio_route` (TV / pad / both) and
`pad_volume` on the TV settings screen decide where decoded audio *plays* — the mic
is independent of the route: playback and mic capture share one headset-enablement
gate (≤2 concurrent headsets, on the shared adapter's iso bandwidth), and the mic
path enables a present headset itself, so it works even on a TV-only route.

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

## Configuration surface

The config is deliberately broad, not minimal: nearly every encoder, capture,
audio and input knob is exposed through the web UI (an *Advanced* disclosure
hides the ones most installs never touch) and persisted in `StreamConfig` /
`InputConfig`. `SessionSettings` (in `sunburst-net`) is the seam — the web
handler fills it from `Config::effective(running_app)` on every `Hello`, so a
change takes effect on the **next session**, live, with no restart and nothing
frozen at startup. Per-app overrides stay small on purpose: codec, bitrate and
preset; everything else is global.

The client asks, the server decides. The TV settings screen can request a codec
and a bitrate ceiling (`Hello.prefer_codec` / `max_bitrate_kbps`, see
PROTOCOL.md) and set purely-local presentation prefs (jitter depth, cursor
overlay, performance hint). Requests are advisory: `negotiate_codec` only honours
a preferred codec the device's `codecs` bitmask already offers, and the bitrate
is the minimum of the server setting, the codec ceiling, the decoder's quirks
hint and the client's ask (`sunburst_net::rate::session_bitrate`, host-tested).

**Breadth stops exactly where the settled decisions are.** These are not missing
knobs to be added later; they are the architecture above:

- **No encryption toggle.** Video/audio are unencrypted by design (LAN-only, raw
  UDP, no QUIC); input and control are always authenticated. There is nothing to
  switch, so there is no switch.
- **The NVENC preset knob is P1–P4, inside `TUNING_INFO_ULTRA_LOW_LATENCY`
  only.** No UHQ, no B-frames, no lookahead — those buy compression with the
  latency this project exists to remove. Rate control is CBR/VBR; both stay in
  the ULL envelope.
- **NvFBC stays opt-in** (`capture_backend`), never the auto default; `Auto` is
  WGC/DDA per the OS.

The whole surface is host-tested through the `Host` trait and `Fake` — the
settings round-trip, the audio-device picker, the per-app override merge — so it
is exercised on the Linux box, not only the 4070.

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
  One checkbox per install beats a guess that drifts. `disable_epp` does it for
  the session, and also pins the pointer speed to 10 (1:1) so the only gain is
  our own sensitivity. The client predicts its cursor overlay from that gain
  (`SessionConfig.pointer_gain_milli`), and the server reports 0, "do not
  predict", while EPP is on.
- **Scale relative deltas with a carried remainder** (`keymap::MouseScaler`),
  never by rounding each event. A slow mouse sends ±1: rounded, 0.5 comes out
  as ±1 (full speed) and 1.5 as ±2.
- Pads are HIDMaestro UMDF2 device nodes carrying reports built in Rust
  (`sunburst-input/src/pad/`), not ViGEmBus. That reversed the Phase 2 plan
  (`b7d971f`); `HARDWARE_TESTING.md` §8 keeps the reasoning.
- Steam Input grabs virtual pads and presents its own emulated device. Usually
  transparent; occasionally double-enumerates. Test this path early — it presents
  as "controller does nothing in one specific game".
- Kernel anti-cheats blocklist some virtual-pad drivers; ViGEmBus is a known
  case, and HIDMaestro's driver is untested against them. `SendInput` sets
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
- **A still desktop yields no frames.** DDA and WGC deliver only on change, so an
  idle screen reads as a low frame rate and a stalled stream. Test with motion
  (a 60 fps video, testufo) or the numbers mean nothing. Two wrong conclusions
  came from this before it was written down.

**NVENC**
- The FFI overlays are transcribed from `nvEncodeAPI.h`, and a wrong size or
  offset fails far from its cause. A 4-byte ME-hint struct that should have been
  16 zeroed `tuningInfo` and made every init fail with code 8. A 4-aligned union
  put the HDR SEI pointers 4 bytes off and crashed the driver. **Lock every
  overlay field you touch with `offset_of!`/size asserts** against the 13.1
  header; they run in the Windows CI test job.
- Surface `nvEncGetLastErrorString` on failure. A bare `NV_ENC_ERR_INVALID_PARAM`
  hides the one sentence that names the field.
- 10-bit needs `inputBitDepth`/`outputBitDepth = NV_ENC_BIT_DEPTH_10` in SDK 13.x,
  not `pixelBitDepthMinus8`. Without it init succeeds as 8-bit and
  `nvEncRegisterResource` rejects the P010 surface.

**Timing**
- Windows timed waits round to the ~15.6ms default tick unless the process holds
  `timeBeginPeriod(1)` (the server does, with Windows 11's hidden-window
  opt-out). A pacer or governor that mysteriously wakes late is this first.

**HDR**
- Windows 10 HDR is a **global display toggle**, not per-app like Win11. It is
  toggled around the session and restored on disconnect by `DisplayGuard`
  (`sunburst-server/src/display.rs`): `DisplayConfigSetDeviceInfo` with the
  advanced-color-state packet on every active output, snapshotted and restored on
  the guard's `Drop` so an abnormal teardown still restores it. Users notice if
  it does not restore, which is why restore is in a guard, not a code path.
  Resolution matching (`ChangeDisplaySettingsExW`) and the optional MikeTheTech
  virtual display live in the same module, driven by the `match_resolution`,
  `virtual_display` and `capture_output` config flags — the VDD is **consumed**
  (device enable/disable via SetupAPI + capture output selection), never authored
  or vendored, so there is no WDDM signing or GPL entanglement.
- On an HDR desktop, capture yields scRGB linear FP16. Shader: normalise by 80
  nits → BT.2020 primaries → PQ EOTF⁻¹ → 4:2:0 subsample. Get chroma siting right
  or UI text fringes. **Not every frame is FP16:** DDA on an SDR desktop yields
  8-bit sRGB, which the shader must linearise first. HDR is detected from the
  output's `DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020`, not from the scRGB
  composition space. That wrong check once made colour correct on HDR desktops
  only. The convert and colorimetry follow the captured state per frame, so an
  HDR↔SDR flip rebuilds the encoder.

**AV1 is not a flag on the HEVC path**
- OBUs, not NAL units. No Annex-B start codes. Packetize on OBU boundaries;
  respect `obu_has_size_field`.
- MediaCodec `csd-0` is an **av1C record**, not raw headers. Marker/version byte,
  `seq_profile`, `seq_level_idx`, `seq_tier`, bit-depth flags, then the sequence
  header OBU. Wrong av1C = decoder configures fine and silently outputs nothing.
  **Build it from the parsed sequence header, never from config.** NVENC signals
  High tier at 4K, and a hard-coded Main tier contradicted the OBU it wrapped.
- Under `enableSubFrameWrite`, NVENC keeps re-reporting stale tile bytes after
  the frame is complete, so `bitstreamSizeInBytes` grows past the real frame.
  **Bound the read by the OBUs**: stop at the tile group whose `tg_end` is the last
  tile. Draining by byte count once sent frames 16× over budget.
- Tiles replace slices for subframe packetization. Start at 2×2 for 4K. The
  `slices` setting counts units per frame for every codec (4 = 2×2 tiles); the
  grid comes from the AV1 spec's uniform spacing, which at 4K can give fewer
  rows than asked (8 → 7), so the drain uses the derived grid, never `2^log2`.
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
- **Configure the decoder from each `CodecPrivate`, not once.** The server
  re-sends it after every encoder rebuild, and an HDR↔SDR flip of the desktop
  changes the stream's colour mid-session. `reconfig::classify` decides whether
  a rebuild changed anything the decoder sees; the `KeyframeGate` drops frames
  older than the build's `first_frame` and holds everything until a keyframe of
  it, asking for one when that IDR was already spent on the old configuration.
  It also covers the session's start: the startup IDR goes out during
  negotiation and is always lost.

**Pads (Phase 8, `sunburst-gip-bridge`)**
- **GIP is vendored, not authored.** The xow/xone C++ (MT7612U radio + GIP +
  wired) is proven against this exact adapter and the pad-audio security handshake
  is unverifiable off-hardware — "vendor now, Rustify later", the same
  consume-a-driver posture as HDR/VDD/NvFBC. Don't re-transcribe it from the spec;
  the Rust rewrite is a deferred follow-up against a *captured corpus*.
- **The seam is a C FFI Rust calls, not JNI.** Kotlin owns only the Activity and
  the `UsbManager` fd; the driver's JNI upcalls were replaced by a C++ `GipSink`.
  The GIP handshake crypto (RSA-PKCS#1v1.5 + ECDH-P256) is reimplemented in Rust
  (`crypto.rs`), host-tested byte-exact — no Java/mbedtls dependency remains.
- **Firmware is fetched, never committed** (`scripts/fetch-firmware.sh`,
  `FW_ACC_00U.bin`); the embedded blob is not vendored. GPL-2-or-later; provenance
  in `sunburst-gip-bridge/vendor/UPSTREAM.md`. The four `[MS-GIPUSB]` defects the
  original plan meant to fix in-place are **audited and resolved** there — three
  were already fixed by the moonlight-trexx port (rumble percentage, extended
  status, uniform signed sticks) and the fourth (host capability advertisement) is
  not a spec requirement (§1.7 negotiation is "None").
- **The headset mic is 24 kHz mono**, distinct from the 48 kHz-stereo speaker
  (`[MS-GIPUSB]` §3.2.5.1.2 format codes); `audio_format(pad)` reports the render
  format, `mic_format(pad)` the capture format. Don't assume they match. The mic is
  Opus-encoded at its native rate and resampled by the server's decoder.
- Rumble carries **four** motors (two rumble + the two Xbox impulse triggers); the
  driver renders the pad's negotiated headset format itself, so `audio_out` takes
  48 kHz stereo regardless.

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

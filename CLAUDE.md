# Sunburst — low-latency 4K HDR game streaming

Rust game streaming server for Windows, with a dedicated Android TV client.
Replaces Sunshine/Moonlight for a two-device household. Not a general-purpose product.

## Hardware targets (fixed — do not design for anything else)

**Server**
- Windows 10 1903+ and Windows 11
- RTX 5070 **non-Ti** (GB205, Blackwell, 9th-gen NVENC)
- **1 NVENC + 1 NVDEC.** No Split Frame Encoding (needs 2+ encoders).
- NVIDIA Video Codec SDK 13.0+, driver r570+. Version-check at startup; older
  headers lack the AV1 GUIDs and Blackwell caps.

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
- 4:2:2 and MV-HEVC (Blackwell additions) are irrelevant; Android decoders want 4:2:0.

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
| DWM composition | ~16.7ms | Only NvFBC/hooking avoids this |
| Capture acquire | 0.5–2ms | |
| scRGB→P010 shader | 0.8–1.5ms | |
| NVENC HEVC P1 ULL | 5–9ms | Fixed floor; no SFE on non-Ti |
| NVENC AV1 P1 ULL | 6–10ms | |
| Packetize + send | <0.5ms | USO offload |
| Wire @ 1GbE | 1–3ms | |
| Jitter buffer | 0–8ms | Adaptive |
| MediaCodec decode | 8–16ms | |
| Panel | 1–3 frames | Game Mode mandatory |

Honest glass-to-glass: **60–100ms**. Sub-40ms claims elsewhere measure
capture-to-wire, not what the eye sees. Do not chase them.

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
sunburst-android/  cdylib + JNI shim
android/           Gradle project; Kotlin owns Activity + SurfaceView only
spikes/            Phase 0 throwaway. Deletable by design.
```

`sunburst-capture`, `-encode`, `-audio`, `-input` and `-server` are
`#![cfg(windows)]` at the crate root, so they compile to nothing on a Linux
host. `sunburst-core` and `-net` are cross-platform: the client needs the
protocol types and the receive half.

## Build and test

Development happens on Linux; the server runs on Windows. Both halves are always
checkable, and neither command lies about the other.

```bash
cargo test --workspace                            # everything host-testable
cargo clippy --workspace --all-targets            # SAFETY comments are enforced
cargo bench -p sunburst-core --bench instr        # record() must stay under 50ns

cargo xwin check --target x86_64-pc-windows-msvc  # the five Windows crates
cargo build -p sunburst-android --target aarch64-linux-android
cargo build -p sunburst-android --target armv7-linux-androideabi
./gradlew -p android assembleDebug testDebugUnitTest
```

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

Real builds, NVENC, and every hardware measurement happen on the 5070 box.
Cross-checking here catches compile errors without a round trip; it proves
nothing about behaviour.

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
- `SendInput` targets the calling thread's desktop. Helper process must live in
  the interactive session (`WTSGetActiveConsoleSessionId` → `CreateProcessAsUser`)
  and re-attach via `OpenInputDesktop`/`SetThreadDesktop` on desktop switch.
- Enhanced Pointer Precision applies an accel curve to injected relative deltas.
  Compensate or document.
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
  `SUPPORT_REF_PIC_INVALIDATION` and `SUPPORT_INTRA_REFRESH`. Do not assume parity.

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

# Roadmap

Phases are ordered so that each one is independently testable and the
highest-risk unknowns resolve earliest. Do not start a phase until the previous
phase's acceptance criteria pass.

---

## Phase 0 — Spikes (2 days)

Throwaway code. The point is to resolve architecture-invalidating unknowns before
committing to any of them.

The probe outlived the phase. Its NvFBC implementation was promoted into
`sunburst-capture` (commit `dd805a7`), and the probe itself moved to
`tools/probe-windows` rather than being deleted with `spikes/`: it still holds
the hardware harnesses the checklist runs and nothing else has — NvFBC status
probing, the NVML second-session query, the present→capture latency harness
(§7) and the direct-pad baseline (§8).

### 0.1 NvFBC availability — **closed: available, GPU-resident, kept as an option**
NVIDIA deprecated NvFBC on the Windows side of the Capture SDK and directs Windows
developers to Desktop Duplication. The instruction was to assume it unavailable
until proven otherwise on this exact driver, and the first probe run appeared to
confirm that: `NvFBC64.dll` loads but exports no `NvFBCCreateInstance`.

**That was the wrong question, not a negative answer.** `NvFBCCreateInstance` is
the 7.x/Linux-shaped API; the Windows one is the legacy `NvFBC_CreateEx` /
`NvFBC_GetStatusEx` / `NvFBC_Enable` set, which the driver does still export. The
probe now walks that path, and because legacy NvFBC is gated to professional
cards by a private-data key, it asks **unkeyed and keyed and reports the pair** —
a keyed success means nothing except beside an unkeyed failure.

And the third possibility turned out to be the real one: not "available" or
"absent" but **present and switched off**. Keyed `CreateEx` succeeds where unkeyed
fails, on this exact driver, for a 3840x2160 session.

Then it was measured properly — GPU-resident via `NvFBCToCuda`, against a
same-window Desktop Duplication control, in both colour modes. NvFBC came in at
0.86–0.94× DDA every time and never once above it, and the frame genuinely stays
on the GPU (0.15–0.19ms per grab against ToSys's 3.6ms), so this is not a badly
built test.

**Neither branch is taken, because the branches asked about throughput and this
project is not optimising throughput.** That result was written up as "delete the
backend", which was broader than the evidence: latency was never measured, and
DDA itself only reached ~38–40 frames/sec on a 144Hz display in every run, so
neither path was ever stressed. NvFBC also grabs in 0.15ms where DDA's
`AcquireNextFrame` plus copy has no number at all.

So **NvFBC is retained as an opt-in backend complementing DDA and WGC**, and
moves into Phase 3 where the `Capture` trait is built. It is never the default —
it needs an undocumented private-data key and is deprecated below this project's
OS floor — but DDA goes black on DRM-protected content and dies on the secure
desktop, so a second GPU-resident path is resilience as much as speed. What
decided its real worth was the present→capture latency harness, not the throughput
numbers above — and it answered: NvFBC is **1.2–1.5ms worse** than DDA and WGC
(`HARDWARE_TESTING.md` §7). So it is kept for resilience alone.

That it took seven runs to get a number worth trusting is the more useful lesson,
and `HARDWARE_TESTING.md` §1 keeps the wrong turns alongside the answer: an idle
desktop that made both paths look identical, a polling loop that measured our own
asking rate, a status bit that had drifted meaning between SDK versions, a setup
struct whose *generation* rather than layout was rejected, an SDR conclusion that
HDR undercut, and a `cuCtxDestroy` on a context we did not own that killed the
probe mid-measurement.

**Exit criteria:** codec matrix confirmed by evidence, NvFBC decision made, quirks
table seeded.

**All three exit criteria are met, and Phase 0 is closed.** The codec matrix is
measured. The NvFBC decision is made — available, GPU-resident, retained as an
opt-in backend. The quirks table is seeded on both boxes: HEVC Main10 at 4K60 on
the Shield, and `c2.amlogic.av1.decoder` with AV1 Main10, HDR10, level 5.1,
`FEATURE_LowLatency` and 4K60 on the Homatics.

0.3 is closed on the narrower question it was really asking. The Homatics is on
Wi-Fi with its gigabit port unused, so the 494 Mbps measured is not a PHY
reading — but it does rule out the ~80 Mbps ceiling that would have made AV1's
efficiency load-bearing, which is what 0.3 existed to catch. The wired
confirmation waits for a cable and blocks nothing.

Two things carried forward rather than closed: Amlogic's max AV1 width is
**3840, not 4096**, so anything rounding a width up fails on that box and not on
the Shield; and the Homatics HEVC decoder *enumerates better than its AV1 one*
while being the path 0.4 struck, which no amount of enumeration can detect.

---

## Phase 1 — Instrumentation (3 days)

First, so every subsequent phase is measurable. Building this later means
retrofitting timestamps through code that already works, and nobody does that.

- Lock-free SPSC ring, preallocated, fixed-size records: `(stage_id: u8,
  frame_id: u32, qpc: u64)`. No allocation, no formatting, no locks on push.
- `QueryPerformanceCounter` throughout; convert to ns only in the drain thread.
- Drain thread at low priority. Aggregates p50/p95/p99/max per stage over a
  rolling window.
- Timestamps ride with the frame through the pipeline and cross the wire in the
  packet header, so the client can attribute end-to-end.
- CLI/TUI readout of the current per-stage table.
- Export a before/after diff format suitable for pasting into a PR.

**Acceptance:** a synthetic 5-stage pipeline reports stable p99s; pushing a record
costs <50ns; zero allocations after warmup (verify with a counting allocator).

---

## Phase 2 — Input (1 week)

No video. Independently testable in a real game. Front-loaded because the
session/desktop plumbing is where "works on my machine" goes to die, and finding
that out now is much cheaper than finding it out in Phase 5.

- UDP socket, single port.
- Authenticated input/control packets: keyed MAC + sequence number, replay rejection.
- Minimal reliable channel (seq + ack + retransmit, ~150 lines) for control messages.
- Gamepad: **HIDMaestro's UMDF2 driver, with the reports built in Rust.** This
  was first planned as ViGEmBus. ViGEmBus was archived on 2 November 2023, so five
  alternatives were surveyed, and HIDMaestro was set aside because its reports
  were built by 53KB of C#, which would have put .NET on the input path. That
  objection went away once the report codec was ported to Rust byte for byte
  (`sunburst-input/src/pad/`, proven against HIDMaestro's 63 golden hashes). So
  the decision was reversed (`b7d971f`): a pad presents as its own family rather
  than always as an X360. `libvirtualhid` fails on licence; `inputtino` and
  WinUHid do not apply. See `HARDWARE_TESTING.md` §8. Rumble callback → forward
  to client.
- Keyboard via scancode `SendInput` (see CLAUDE.md traps).
- Mouse: absolute mode (`MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`,
  normalised 0–65535) and relative mode. Wheel + horizontal wheel + XBUTTON1/2.
- ~~Session helper: spawn into interactive session, survive fast user switching~~
  — **struck.** The no-service decision (Phase 9) made it obsolete: the server
  already runs in the interactive session, so `WTSGetActiveConsoleSessionId` →
  `CreateProcessAsUser` is gone entirely. What survives is smaller and different,
  and is kept below.
- Desktop re-attach: `OpenInputDesktop`/`SetThreadDesktop` when the **desktop**
  switches under UAC or the lock screen. Within one session, not across two.
- `tools/fakeclient` to drive it during development. Kept rather than throwaway:
  it is the only end-to-end exercise of the transport that runs without a
  Windows box, it is what CI runs, and Phase 4 will point it at the video path.

**This phase lands in two chunks**, because it splits unevenly by what can be
verified. The protocol and transport half is testable on the development
machine; the virtual pads and `SendInput` are testable on none of it.

1. **Control channel and transport** *(done)* — control-message payloads, the
   reliable layer, the UDP endpoint, and the fake client.
2. **Input injection** — scancode `SendInput`, mouse modes, desktop re-attach
   *(done)*. Attaches at the `on_input` seam in `sunburst_net::ControlHandler`.
3. **Gamepad and rumble** *(landed; driver decision reversed)* — the report
   stack is ported (`sunburst-input/src/pad/`), `inject.rs` routes
   `InputEvent::Gamepad`, and the `Outbound` seam on `Endpoint` carries rumble
   and rich `PadOutput`. The driver question did **not** settle on ViGEmBus:
   commit `b7d971f` stands up the HIDMaestro UMDF2 virtual-pad device nodes
   instead ("No ViGEmBus fallback, by decision"), so a pad presents as its
   native family rather than always an X360. Box validation (a real Steam game,
   the UAC/lock cycle, Steam Input) is `HARDWARE_TESTING.md` §6/§8.

**Acceptance:** gamepad, keyboard and mouse all work in a real Steam game launched
from Big Picture. Input survives a UAC prompt and a lock/unlock cycle. Steam Input
interaction characterised and documented.

---

## Phase 3 — Capture → encode → file (1 week)

Still no network. Output is an elementary stream on disk, verified by playback on
the actual TVs.

**Status: landed and streaming on the 4070; TV playback pending.** All three
backends are in `sunburst-capture`, and NvFBC's convert kernels are vendored PTX.
H.264, HEVC and AV1 encode on the box, and the colour signalling (VUI, the AV1
`color_config`, the ST 2086 SEI) is checked from the dumps (`HARDWARE_TESTING.md`
§9). First contact broke NVENC init, the HDR path and the AV1 drain, and all
three are fixed. The measured p99s are in §4: encode about 9–11ms, convert about
0.1ms. What remains is playback on the TVs and the capture→encode p99 over a
sustained run.

- `Capture` trait + `Caps`. `AccessLost` recoverable at any point.
- **DDA** backend: blocking `AcquireNextFrame` on a dedicated thread, release
  immediately after taking the texture reference.
- **WGC** backend: free-threaded frame pool, `R16G16B16A16Float` for HDR.
- **NvFBC** backend, opt-in: promoted out of the Phase 0 probe (now
  `tools/probe-windows`), keyed `CreateEx` → `NvFBCToCuda`, and it **stays
  CUDA-native**. The trait yields `Frame::Cuda`, a CUDA convert kernel produces
  P010/NV12, and NVENC takes it as `CUDADEVICEPTR` input — never bounced through
  D3D11 (CLAUDE.md, *Architectural decisions*). Never selected automatically.
- Backend selection: WGC on Win11, DDA on Win10, fall back to the other on
  `AccessLost` or repeated black-frame detection. NvFBC only when asked for.
- Colour convert, chosen per frame from the captured desktop's real HDR state:
  P010 BT.2020 PQ (HDR), P010 BT.709 (SDR on HEVC/AV1), or NV12 BT.709 (H.264).
  An 8-bit sRGB desktop is linearised in the shader.
- NVENC init: HEVC Main10 and AV1 Main10, P1–P4, `TUNING_INFO_ULTRA_LOW_LATENCY`,
  CBR, no lookahead, no B-frames, infinite GOP.
- **Subframe readback** — slices for HEVC, tiles for AV1. Not optional (see CLAUDE.md).
- HDR metadata extraction (mastering primaries, MaxCLL/MaxFALL).
- Dump Annex-B (HEVC) and OBU stream (AV1) to disk.

**Acceptance:** both streams play correctly on their target device with HDR
active. Capture→encode p99 within budget. Survives alt-tab, resolution change,
and a Big Picture game launch/exit without a permanent stall.

---

## Phase 4 — Transport (2 weeks)

**Status: landed, box-validation pending.** The protocol logic is host-tested
(session negotiation and per-session keys, the zero-allocation reassembler and
jitter buffer, NACK-driven retransmission, the HEVC/AV1 reference-invalidation
state machines, delay-gradient rate control, the deadline pacer and USO
batching). The encoder gained force-IDR, `inputTimeStamp`, invalidation and
seamless bitrate reconfigure; the server starts a session on `Hello` and drives
recovery, rate control and paced/offloaded send. `fakeclient stream` is the stub
receiver and the play-on-TV dump. The first box runs streamed 4K HEVC and AV1
to it. They fixed the AV1 NACK storm (6229 → 0), a pacer feedback loop (send p99
1,275ms → single-digit ms) and the rate caps, and they recorded send p99s in
`HARDWARE_TESTING.md` §4. What remains is the rest of §9: sustained runs,
induced loss, `tc` convergence, and USO on vs off.

- Packetization per PROTOCOL.md. MTU-safe, ≤1200 byte payload.
- **USO send offload** (`WSASetSockopt(UDP_SEND_MSG_SIZE)`) and URO receive
  (`UDP_RECV_MAX_COALESCED_SIZE`). ~85x syscall reduction at 5,200 pkt/s. This is
  the single biggest CPU win in the transport and the thing that would otherwise
  make naive raw UDP lose to a tuned QUIC stack.
- Pacing against the **frame deadline**, not RTT. Keep pacing even with 1GbE
  headroom — bursty UDP overruns switch buffers regardless of link speed.
- Jitter buffer, adaptive depth.
- NACK + `NvEncInvalidateRefFrames`. Separate state machines for HEVC and AV1.
- Intra-refresh, no periodic IDR — subject to the quirks table.
- Delay-gradient rate control (measure one-way delay *gradient*, not loss — reacts
  before queues build). Applies via `NvEncReconfigureEncoder`, which changes
  bitrate without tearing down the session.

**Acceptance:** sustained 4K60 at target bitrate to a stub receiver. Induced 2%
packet loss recovers without a visible hitch and without an IDR. Rate controller
converges within 2s of a bandwidth change and does not oscillate.

---

## Phase 5 — Android client (2 weeks)

First glass-to-glass number.

**Status: landed, box-validation pending.** The client is built and packaged:
the Rust cdylib for both ABIs inside the APK, pairing from the TV, decode to a
`SurfaceView` via the `ndk` crate's AMediaCodec, vsync-timed present,
keyboard/mouse/gamepad input, HDR static info, `setFrameRate`, a
`PerformanceHintManager` session, and client-side cursor rendering from
server-side capture. What remains is the box run — HARDWARE_TESTING §10. The
pure maps (keycode/axis/HDR/PIN) are host-tested; the device work is
`cargo xwin`/`assembleDebug`-verified and clippy-linted on the Android target,
not yet measured. **Build tooling:** the `rust-android-gradle` plugin turned out
incompatible with the latest AGP (it uses the removed `AppExtension`), so a small
hand-rolled Gradle task runs the same `cargo build` CI uses and stages the `.so`;
AGP is on 9.4.0.

- Rust `cdylib`, both ABIs. Network, depacketization, jitter buffer, decode
  driving all in Rust via the `ndk` crate. Kotlin owns only Activity + `SurfaceView`.
- Codec selection from the Phase 0 enumeration; quirks sent at handshake.
- MediaCodec: Surface output, `KEY_LOW_LATENCY`, `KEY_PRIORITY=0`, high
  `KEY_OPERATING_RATE`, vendor low-latency keys where present.
- av1C construction for AV1 `csd-0`. HEVC `csd-0` from VPS/SPS/PPS.
- `Surface.setFrameRate()`; `Choreographer`-paced `releaseOutputBuffer`.
- HDR: `MediaFormat.KEY_HDR_STATIC_INFO`, check `Display.HdrCapabilities`.
- Input: `requestPointerCapture()` + `onCapturedPointerEvent` for mouse;
  `onKeyDown`/`onKeyUp` + `onGenericMotionEvent` for gamepad; `onKeyPreIme` so the
  IME doesn't swallow keys. Enumerate `getMotionRanges()` rather than assuming
  deadzones.
- Client-side cursor rendering.
- `PerformanceHintManager` behind an API-31 check.

**Acceptance:** 4K60 HDR on both devices. Glass-to-glass measured (high-speed
camera or LED-on-input rig) and within the 60–100ms budget. No microstutter over
a 30-minute session.

---

## Phase 6 — Audio (1 week)

**Status: landed, box-validation pending.** End to end: WASAPI loopback capture
+ Opus encode on the server, one Opus frame per unauthenticated packet on the
wire, Opus decode + low-latency AAudio playback on the client. Host-tested where
pure (the Opus codec wrapper round-trips, the PCM conversion/accumulator, the
SPSC playback ring, the audio packet), `cargo xwin`-verified on Windows and
built + clippy-linted on both Android ABIs. What remains is the box run —
HARDWARE_TESTING §11.

- **WASAPI loopback** via `IAudioClient3` on a **selectable** render endpoint:
  the default endpoint (host audible) by default, or a named one — set
  `StreamConfig.audio_device` to "Steam Streaming Speakers" to reuse Valve's
  signed virtual sink and silence the host without a driver of our own. The
  server injects silence during the endpoint's idle gaps so the cadence holds.
- **Opus** at 48 kHz stereo, 5 ms frames, `RESTRICTED_LOWDELAY`, in-band FEC on.
  The codec is `unsafe-libopus` (libopus transpiled to pure Rust) — chosen over
  audiopus/CMake so it cross-compiles to Windows and both Android ABIs as plain
  Rust. **No audio NACK:** FEC + PLC recover a lost packet more cheaply than a
  retransmit.
- **AAudio** `LowLatency`, fed from a lock-free PCM ring; underruns play silence.
- **A/V sync** rides the existing Phase 1 timestamps: audio and video share the
  server's performance-counter domain, and audio buffering is kept bounded (drop
  the newest frame past a watermark, silence on underrun) so the offset cannot
  drift without limit. The exact buffer target is a box-tuned number.
- **Audio instrumentation:** two new `Stage` chains — capture/encode/send on the
  server, recv/decode/play on the client — so audio latency is measurable
  alongside video.

**Deferred (defended):** a custom virtual-sink driver (the selectable Steam
device covers host-mute), arbitrary sample-rate resampling (the endpoint is
required at 48 kHz, warned otherwise), surround (stereo downmix), and
microphone/return audio (server → client only).

**Acceptance:** no drift over 30 minutes. Audio latency measured and reported
alongside video.

---

## Phase 7 — Optional backends and polish

Ordered by value, not difficulty.

**Status: landed, box-validation pending.** App launching already existed
(`CreateProcess`/`ShellExecute` for `steam://`, `schtasks` autostart, prep/undo).
This phase added the display integration that was missing and the optional
virtual display:

- **Native HDR + resolution control** (`sunburst-server/src/display.rs`),
  replacing the fake `set-hdr` shell placeholder. A `DisplayGuard` created in
  `session_start` toggles advanced color (HDR) via `DisplayConfigSetDeviceInfo` on
  every active output, and — opt-in (`match_resolution`) — switches the primary
  mode to the client's resolution via `ChangeDisplaySettingsExW`; its `Drop`
  restores the exact prior state, so a disconnect **or an abnormal teardown**
  puts the desktop back.
- **Per-app profiles applied live.** `WebHandler::on_hello` resolves
  `Config::effective(running_app)` and passes the bitrate/codec through
  `StreamControl::session_start`, so a launched game's profile reaches the stream;
  `WinHost::running_app` reaps an exited game (via `try_wait`) and undoes its prep.
- **Optional virtual display**, consuming the MikeTheTech VDD (`virtual_display`,
  off by default — the NvFBC posture). `display::VirtualDisplay` enables the
  installed driver's device node via SetupAPI for the session and disables it on
  drop; capture targets a chosen output (`capture_output`, DXGI output index)
  through the new `OutputSelect` on both DDA and WGC. Absent driver ⇒ warn and
  fall back to the physical display. We **consume** the VDD (device state +
  capture only), never author or vendor a driver — no EV-signed WDDM work, no GPL
  linkage, the ViGEmBus/HIDMaestro posture.

What remains is the box run — HARDWARE_TESTING §12. The display and VDD code is
`cargo xwin`-verified; behaviour (which output is the virtual one, the VDD's exact
hardware id, HDR/mode restore) is still to be confirmed on the 4070.
- ~~**NvFBC**~~ — **moved to Phase 3** as an opt-in backend. Phase 0.1 unlocked
  it, made it GPU-resident via ToCuda and measured 0.86–0.94x DDA, which settles
  throughput and nothing else. It belongs beside the other backends rather than
  in the optional-polish phase; see `HARDWARE_TESTING.md` §1.
- ~~**Swapchain hooking**~~ — **struck on measurement.** This existed to get a
  pre-composition frame, and composition has now been priced: **~4ms at 144Hz,
  ~8ms at 60Hz** — about half the desktop's refresh interval, not the 16.7ms the
  budget claimed (`HARDWARE_TESTING.md` §7). Hooking is per-process, must chase
  each Big Picture launch into a new window, needs separate paths for
  `IDXGISwapChain::Present`, `vkQueuePresentKHR`, `wglSwapBuffers` and D3D9
  `EndScene`/`Present` — where plain D3D9 cannot share surfaces and needs a
  `StretchRect`→sysmem→upload path — and carries an anti-cheat warning. That was
  arguable against 16.7ms. It is not arguable against 4ms.

  Left visible rather than deleted, because the reasoning is the point: the item
  was never wrong to plan, it was wrong to plan *unmeasured*. **The cheaper win is
  a display setting** — running the server's desktop at a high refresh rate buys
  ~5ms with no code at all.
- **Non-Steam game launching** — any exe or URI entry already launched. What
  was missing was knowing when such a game is running. A launcher stub exits
  after starting the game, which reaped the app (and undid its prep) while the
  game was on screen; a URI had no process at all and blocked every later
  launch. Now each launch runs in a job object, an entry can name the game's
  own `wait_process` for a hand-off that leaves the job, and an untracked URI
  is replaced by the next launch (`sunburst_web::apptrack`, host-tested; §5).

---

## Codecs — HEVC / AV1 / H.264

**Status: H.264 landed, box-validation pending.** Alongside HEVC Main10 and AV1
Main10 (the HDR codecs), the encoder gained **H.264 High, 8-bit SDR** — an
opt-in, low-latency codec. NVENC has no 10-bit H.264, so it never carries HDR;
`negotiate_codec` picks it only by explicit preference or as a client's sole
offer, never over an HDR codec. It reuses the HEVC packetizer, sequence-header
path and reference-invalidation `Window`; its one new piece is an 8-bit SDR
convert (scRGB→NV12 BT.709, with an ACES HDR→SDR tonemap) on both the D3D11 and
NvFBC paths, feeding NVENC NV12. See HARDWARE_TESTING §13. Both NvFBC CUDA
kernels (P010 and NV12) are real PTX vendored from `cuda-kernel.yml`.

---

## Phase 8 — Xbox Wireless Adapter — **merged** (PR #2, `af2d876`; CI green) — hardware validation pending

Depended on Phase 5 and nothing else — not audio, not the optional capture
backends. Shipped at its **maximal scope**: both transports — the Xbox **Wireless
Adapter** (`045e:02e6`, MT7612U radio) and **wired** Xbox One/Series pads — up to
**four pads**, buttons/sticks/triggers, **trigger rumble** (the impulse-trigger
motors), **battery**, and **full-duplex headset audio** (server audio → the pad's
headphones, and the pad mic → a Windows microphone, capped at two concurrent
headsets). In-app pairing from the TV remote.

**How it shipped — vendor, not rewrite.** The adapter is not a HID device: an
MT7612U chip that must be given firmware and have a radio brought up before it
speaks to a pad. A *hardware-validated* Android port of xow/xone already existed
(`/home/turk/Git/moonlight-trexx`), and reading it corrected the original premise
of this section. The GIP layer is **not ~450 lines** — it is **~6,100 lines** of
GIP/controller/wired logic plus a byte-exact **RSA-PKCS#1v1.5 + ECDH-P256**
security handshake (the exact thing `AUDIO.md` records costing four hardware
iterations), on top of ~3,460 lines of radio C++ vendored either way. Rewriting
that blind from the spec, with the pad-audio handshake unverifiable off-hardware,
was the wrong first move. So the decision was **"vendor now, Rustify later"**:

- **Vendored xow/xone C++** (MT7612U radio + GIP + wired + libusb) behind the new
  **`sunburst-gip-bridge`** crate — a **C FFI seam Rust calls** (not the upstream
  JNI), with a safe `Bridge` API and a pure-Rust **host stub** so everything but
  the device is dev-box-verifiable. GPL-2-or-later; provenance in
  `sunburst-gip-bridge/vendor/UPSTREAM.md`.
- **GIP handshake crypto reimplemented in Rust** (`crypto.rs`, RustCrypto),
  host-tested byte-exact to the reference — the first real step of the eventual
  Rustification, and it removed the vendored driver's Java/mbedtls dependency.
- **USB**: Kotlin holds `UsbManager` permission and passes
  `UsbDeviceConnection.getFileDescriptor()` down; Rust wraps the fd. No JNI on the
  input path — Kotlin owns only the Activity, per CLAUDE.md.
- **Firmware**: `FW_ACC_00U.bin`, fetched by `scripts/fetch-firmware.sh` at build
  time and never committed (needs `bsdtar`/`cabextract`); the embedded blob is not
  vendored.
- **The virtual mic is consumed, not authored**: the server renders the pad mic to
  a signed "Steam Streaming Microphone" endpoint selected by name — the inbound
  twin of the "Steam Streaming Speakers" reuse, no driver of ours.

**The GIP-in-Rust goal is kept, re-scoped.** Rather than a blind-from-spec rewrite,
it becomes a **captured-corpus follow-up**: once the vendored driver runs on
hardware, record real GIP frames, rewrite the interpretation layer in Rust to
reproduce that verified corpus plus `[MS-GIPUSB]`, and diff it against the C++
on-device before swapping it in behind the same bridge seam. The four
`[MS-GIPUSB]` defects the original plan meant to fix in-place (rumble-as-raw-byte,
a wired sign-extension discontinuity, a dropped extended status message,
unadvertised host capabilities) have been **audited and resolved** — the vendored
moonlight-trexx port already fixed three, and the fourth is not a spec requirement
(§1.7 capability negotiation is "None"); status per defect is in
`sunburst-gip-bridge/vendor/UPSTREAM.md`. The pad mic now works on **every** audio
route, including 'TV only': playback and mic capture share one headset-enablement
gate (the ≤2-headset cap is on enabled headsets, not per direction), and the mic
path enables a present headset itself rather than relying on playback. **The one
remaining Phase 8 follow-up** is the GIP-in-Rust rewrite above (gated on a
hardware-captured corpus).

*(The old "X360 boundary" caveat is moot: the Phase 2 driver reversal emulates via
HIDMaestro device nodes, not a ViGEm X360 pad, and trigger rumble + battery in fact
landed.)*

**Two alternatives to vendoring the adapter were measured and both closed**
(`HARDWARE_TESTING.md` §8). **USB/IP** would have forwarded the adapter to Windows
and let its own driver own it, deleting the vendored radio and GIP entirely — but
`usbip-win2` 0.9.8.0 cannot carry the Xbox GIP protocol: a wired Xbox One pad
fails the same way in both receive modes, with the Windows driver resetting the
interrupt pipe until it gives up and resets the device. The adapter is a strictly
harder case. Revisit only if upstream fixes GIP; it is not a plan. **HIDMaestro**
reaches WGI/GameInput with byte-exact identity; its .NET SDK can be hosted from
Rust (a NativeAOT shim with `[UnmanagedCallersOnly]` exports, or `netcorehost`).
That is orthogonal to this phase — it is the route past the *server-side emulated-
pad* ceiling should a native family ever need to express more than what shipped;
the DS4 target is the other, at the cost of games seeing a DualShock. Gated on
measuring per-frame managed allocation (`HMGamepadState` carries an axes
dictionary) first. Neither bears on the vendored bridge, which stands either way.

**Acceptance (hardware, pending the box — `HARDWARE_TESTING.md` §15):** four pads
pair and play simultaneously through Big Picture, wired and via the adapter;
rumble incl. the trigger motors arrives and stops cleanly, including when the stop
packet is lost; battery shows per pad; a headset plays server audio and its mic
reaches the server's virtual microphone; input latency is measured against a
directly-connected pad and the difference reported.

**Continuing on the 4070 box + real pads.** The dev-box work is merged; what is
left needs the hardware. Sequence (the detailed checklist is `HARDWARE_TESTING.md`
§15 — this is the setup path to it):

1. **Server** — download the `sunburst-windows` artifact from the green `main` CI
   run (its `package` job builds release `sunburst-server` + `fakeclient` + the web
   UI), so nothing is compiled on the box; or `cargo build --release -p
   sunburst-server -p fakeclient`. Start it (it autostarts via the scheduled task
   once configured) and open the web UI.
2. **Firmware** — on a build host run `scripts/fetch-firmware.sh` (needs `bsdtar`
   or `cabextract`); it extracts `FW_ACC_00U.bin` into the gitignored assets path
   so it ships inside the APK. Never committed.
3. **Client APK** — `cd android && ./gradlew assembleDebug` (both ABIs), sideload
   to the Shield (arm64-v8a) and the Homatics (armeabi-v7a), and pair each from the
   TV. Attach the adapter or a wired pad; grant the `UsbManager` permission.
4. **Consumed audio devices** — install Steam so its Remote Play virtual devices
   exist, then set `audio_device` = "Steam Streaming Speakers" and (for the mic)
   `mic_device` = "Steam Streaming Microphone" by name in the web UI. Big Picture is
   the workload.
5. **Run `HARDWARE_TESTING.md` §15** — the pad matrix end to end: wired One/Series
   enumerate and play; the adapter loads firmware, brings up the radio, and pairs
   from the remote; four pads at once; rumble incl. the trigger motors with
   lost-stop self-heal; battery; headset both directions (≤2) at the negotiated
   format (incl. the 24 kHz-mono mic → the Steam mic endpoint); input through a UAC
   prompt and a lock/unlock; Steam Input characterised; latency vs a wired pad.
6. **Record a GIP frame corpus** (the §15 step) — this is the gate for the **one
   remaining follow-up, the GIP-in-Rust rewrite**: capture real input / rumble /
   handshake / capture-render audio frames from the working driver, reimplement the
   interpretation layer in Rust to reproduce that verified corpus plus
   `[MS-GIPUSB]`, and diff it against the C++ on-device before swapping it in behind
   the same `sunburst-gip-bridge` seam.

---

## Phase 9 — Management web UI

**Its first half is a Phase 2 prerequisite and lands before the input work.**
Phase 2 requires authenticated input packets, PROTOCOL.md derives the session key
from a `pairing_secret`, and nothing else produces or stores one. Configuration,
the client store and pairing therefore come first; the rest follows whenever.

Covers clients, sessions, start-up, configuration, and the app catalogue the
client launches from.

- **No Windows service.** Capture and `SendInput` both need the interactive
  session, so nothing useful can live in session 0. The server autostarts at
  logon through a scheduled task — `ONLOGON` with highest privileges, because
  only a task can request those and without them `SendInput` cannot reach an
  elevated window past UIPI. This drops a service, an installer and a session-0
  IPC surface, and makes launching an app a plain `CreateProcess`.
  **With nobody logged in there is no server and no web UI.** Capture could not
  work in that state either, so nothing is lost that a user need not be present
  for.
- **LAN bind, bearer token, plain HTTP.** No TLS: the token is a shared-secret
  gate, not confidentiality on the wire, which is the same posture CLAUDE.md
  already takes for unencrypted video on the same network. A LAN bind with no
  token is refused at load and at save.
- **Manual app entries only.** Big Picture is an ordinary entry
  (`steam://open/bigpicture`), which is most of what Phase 7 asks for.
- **Sessions reuse Phase 1.** `DrainHandle::report()` already produces per-stage
  percentiles; the UI renders those rather than measuring anything again, and
  carries `Report::is_lossy` through so a thinned table cannot be quoted by
  accident.

**Acceptance:** a device pairs from a TV and survives a restart; an app launches
with its prep commands and has them undone on exit; autostart survives a logoff
and a fast user switch; input reaches an elevated window.

### Not implemented, deliberately

- **Steam or other launcher scanning.** No `libraryfolders.vdf`,
  `appmanifest_*.acf`, `shortcuts.vdf`, Epic, GOG or Playnite. Entries are typed
  in once and there are not many of them; the alternative is parsing formats that
  change without warning to save a few minutes of typing.
- **Remote access from outside the LAN.** Plain HTTP, a bearer token,
  PIN-derived pairing and unencrypted video all assume the LAN. Exposing this to
  a WAN would invalidate every one of those at once, not just the transport. If
  it is ever wanted, that is a VPN's job.
- **Multi-user support.** One operator, one token, one config. The server runs in
  one interactive session by design, so a second user is not something it could
  serve anyway.

Box art is **deferred rather than dropped** — the control channel carries names
and ids only, and nothing forecloses adding an HTTP fetch once there is something
to fetch.

---

## Capture backend reference

Full detail in CLAUDE.md traps. Latency is present→capture, measured on a 144Hz
desktop (`HARDWARE_TESTING.md` §7). It works out to about half the refresh
interval, so a higher desktop refresh rate lowers every row.

| Backend | Latency (p50) | Status |
|---|---|---|
| DDA | **3.83ms** | Phase 3, Win10 default |
| WGC | **4.24ms** | Phase 3, Win11 default |
| NvFBC | **5.28ms**, 1.2–1.5ms worse; 0.86–0.94× DDA on throughput | Phase 3, opt-in, for resilience alone (DRM, secure desktop) |
| Virtual display (IDD) | Same capture path as above; solves headless and resolution matching | Phase 7, the MikeTheTech VDD **consumed**, opt-in `virtual_display` |
| ~~Swapchain hook~~ | Would skip composition, which costs only ~4ms at 144Hz | **Struck** on measurement (Phase 7) |

Not implemented, listed so nobody proposes them: GDI `BitBlt`, `PrintWindow`,
Magnification API, `DwmGetDxSharedSurface`, mirror drivers. All CPU-readback,
tens of ms, or dead since Windows 8.

# Roadmap

Phases are ordered so that each one is independently testable and the
highest-risk unknowns resolve earliest. Do not start a phase until the previous
phase's acceptance criteria pass.

---

## Phase 0 — Spikes (2 days)

Throwaway code. The point is to resolve architecture-invalidating unknowns before
committing to any of them.

One piece is no longer throwaway: `spikes/probe-windows`'s NvFBC implementation
is the only working copy in the project and has to be promoted into
`sunburst-capture` in Phase 3 before this directory is deleted.

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
decides its real worth is the present→capture latency harness that follows Phase
0, not the throughput numbers above.

That it took seven runs to get a number worth trusting is the more useful lesson,
and `HARDWARE_TESTING.md` §1 keeps the wrong turns alongside the answer: an idle
desktop that made both paths look identical, a polling loop that measured our own
asking rate, a status bit that had drifted meaning between SDK versions, a setup
struct whose *generation* rather than layout was rejected, an SDR conclusion that
HDR undercut, and a `cuCtxDestroy` on a context we did not own that killed the
probe mid-measurement.

**Exit criteria:** codec matrix confirmed by evidence, NvFBC decision made, quirks
table seeded.

Two of the three are done: the codec matrix is measured, and the NvFBC decision
is made — available, GPU-resident, retained as an opt-in backend. The quirks table is half-seeded — the Shield is enumerated,
the Homatics is not — so **0.2 and 0.3 are all that stand between here and Phase
0 closing**, and both need the Homatics rather than the server.

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
- ViGEmBus gamepad via `vigem-client`. Rumble callback → forward to client.
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
machine; ViGEm and `SendInput` are testable on none of it.

1. **Control channel and transport** *(done)* — control-message payloads, the
   reliable layer, the UDP endpoint, and the fake client.
2. **Input injection** — ViGEm, scancode `SendInput`, mouse modes, desktop
   re-attach. Attaches at the `on_input` seam in `sunburst_net::ControlHandler`.

**Acceptance:** gamepad, keyboard and mouse all work in a real Steam game launched
from Big Picture. Input survives a UAC prompt and a lock/unlock cycle. Steam Input
interaction characterised and documented.

---

## Phase 3 — Capture → encode → file (1 week)

Still no network. Output is an elementary stream on disk, verified by playback on
the actual TVs.

- `Capture` trait + `Caps`. `AccessLost` recoverable at any point.
- **DDA** backend: blocking `AcquireNextFrame` on a dedicated thread, release
  immediately after taking the texture reference.
- **WGC** backend: free-threaded frame pool, `R16G16B16A16Float` for HDR.
- **NvFBC** backend, opt-in: promoted out of `spikes/probe-windows`, keyed
  `CreateEx` → `NvFBCToCuda` → `cuGraphicsD3D11RegisterResource` so the trait
  still yields a D3D11 texture. Never selected automatically.
- Backend selection: WGC on Win11, DDA on Win10, fall back to the other on
  `AccessLost` or repeated black-frame detection. NvFBC only when asked for.
- scRGB→P010 compute shader.
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

- WASAPI loopback via `IAudioClient3` at minimum engine period.
- Virtual sink so the host stays muted.
- Opus encode; libopus via NDK on the client, Oboe/AAudio in low-latency mode.
- A/V sync against the existing Phase 1 timestamps.

**Acceptance:** no drift over 30 minutes. Audio latency measured and reported
alongside video.

---

## Phase 7 — Optional backends and polish

Ordered by value, not difficulty.

- **Big Picture launch** — `steam://open/bigpicture` from the existing interactive
  session helper. Win10 HDR global toggle around the session with restore on
  disconnect. Game launch/exit detection for per-game bitrate profiles.
- **IDD** — virtual display for client-native resolution and headless operation.
  Solves resolution matching and HDR mode control cleanly. Requires an EV-signed
  WDDM driver; strongly consider consuming an existing VDD (Parsec VDD, Virtual
  Display Driver) rather than authoring one.
- ~~**NvFBC**~~ — **moved to Phase 3** as an opt-in backend. Phase 0.1 unlocked
  it, made it GPU-resident via ToCuda and measured 0.86–0.94x DDA, which settles
  throughput and nothing else. It belongs beside the other backends rather than
  in the optional-polish phase; see `HARDWARE_TESTING.md` §1.
- **Swapchain hooking** — opt-in, with an explicit anti-cheat warning. Hook
  `IDXGISwapChain::Present`/`Present1`/`ResizeBuffers`, `vkQueuePresentKHR`,
  `wglSwapBuffers`, and D3D9 `EndScene`/`Present`. D3D9Ex can share surfaces to
  D3D11; plain D3D9 cannot and needs a `StretchRect`→sysmem→upload path that is
  meaningfully slower. Poor fit for Big Picture (per-process, must chase each
  launch) but the only way to beat DWM composition if NvFBC is unavailable.
- **Non-Steam game launching.**

---

## Phase 8 — Xbox Wireless Adapter

Depends on Phase 5 and nothing else — not audio, not the optional capture
backends — so it can be pulled ahead of Phases 6 and 7 at any point.

The adapter (`045e:02e6`) is not a HID device. It is an MT7612U wireless chip
that must be given firmware and have a radio brought up before it will speak to a
pad at all. That half is ~6,500 lines in the xow/xone lineage; GIP itself is
about 450.

- **Radio: vendored C++** behind a narrow FFI seam, per the same reasoning
  CLAUDE.md applies to D3D11/NVENC. Proven against this exact adapter, and
  unforgiving enough that a re-transcription reads as "the dongle does nothing".
- **GIP: Rust, written from [MS-GIPUSB] v20240916** — not transcribed from
  `gip.cpp`. The xow-derived code has four known defects against that spec
  (rumble as a raw byte rather than a percentage, a sign-extension discontinuity
  on the wired path, a dropped extended status message, and capabilities never
  advertised). Transcribing reimports all four.
- **USB**: Kotlin holds `UsbManager` permission and passes
  `UsbDeviceConnection.getFileDescriptor()` down; Rust wraps the fd. No JNI on
  the input path.
- **Firmware**: `FW_ACC_00U.bin`, fetched by `scripts/fetch-firmware.sh` at build
  time and never committed. Needs `bsdtar` or `cabextract`.
- **In-app pairing is required, not a nicety.** The physical pairing button on
  the unit here is dead, and it must be reachable from the TV remote — needing a
  working pad to pair a pad defeats the point.

**Scope is set by what survives the ViGEm X360 boundary:** four pads, buttons,
sticks, triggers, rumble. Motion, trigger rumble and battery-to-host are out —
XInput has nowhere to put them. Battery can still be shown client-side. Pad
headphone audio depends on Phase 6 and is a separate decision.

**Acceptance:** four pads pair and play simultaneously through Big Picture.
Rumble arrives and stops cleanly, including when the stop packet is lost. Input
latency is measured against a directly-connected pad and the difference reported.

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

Ranked by latency. Full detail in CLAUDE.md traps.

| Backend | Latency | Status |
|---|---|---|
| Swapchain hook | Pre-composition, saves ~1 frame, uncapped fps | Phase 7, opt-in |
| IDD | Very good; solves headless + resolution matching | Phase 7 |
| NvFBC | GPU-resident via ToCuda; **0.86–0.94x DDA** on throughput, latency unmeasured | Phase 3, opt-in |
| WGC | Post-composition, refresh-capped | Phase 3, Win11 default |
| DDA | Post-composition, refresh-capped | Phase 3, Win10 default |

Not implemented, listed so nobody proposes them: GDI `BitBlt`, `PrintWindow`,
Magnification API, `DwmGetDxSharedSurface`, mirror drivers. All CPU-readback,
tens of ms, or dead since Windows 8.

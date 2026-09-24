# Hardware testing checklist

Everything in this repo builds and passes its tests on a Linux development
machine that has no GPU encoder, no hardware decoder, and neither client ABI.
That covers correctness of the protocol and state-machine code and nothing else.
This file tracks what only real hardware can settle.

Tick items off as they are verified, and note the device and driver each result
came from. **Anything found broken is recorded here with the symptom before it is
fixed**, so the same case gets re-tested afterwards.

**Legend:** `[ ]` untested · `[x]` verified · `[!]` failed, see note

---

## Test environments

| # | Device | OS / driver | Notes |
|---|---|---|---|
| S | Windows server | NVENC API 13.1 | RTX 4070, 12 GiB (AD104, Ada). One NVENC, one NVDEC. Record the driver version alongside any result. |
| A | NVIDIA Shield TV | Android 11 / API 30 | `arm64-v8a`, Tegra X1, HEVC Main10 only |
| B | Homatics Box R 4K Plus | Android 14 | `armeabi-v7a`, Amlogic S905X4, AV1 Main10. HEVC decoder is broken — do not retest. |

Record which environment each result came from. Several of these behave
differently on the two boxes, and an API-gated feature often reaches only one:
`minSdk` is 30 because of the Shield, so anything at API 31+ helps the Homatics
and does nothing on the Shield.

---

## Reading anything back off the Homatics

**One step first, or every check below silently passes whatever the truth is.**
The box ships with `persist.log.tag=S`, which silences the whole main logcat
buffer — not just one app. Instrumentation that is working looks exactly like
instrumentation that is not.

```bash
adb shell setprop persist.log.tag '""'   # effective immediately, no reboot
adb shell setprop persist.log.tag S      # put it back afterwards
```

Crashes never reach the crash buffer on that box whatever the property says.
Read them from the dropbox instead:

```bash
adb shell dumpsys dropbox --print data_app_crash
```

Its wireless-debugging port moves between sessions. If `adb connect` is refused,
try `:5555` before concluding the box is down; `ping` settles whether it is up.

**Prefer pulling a file to reading logcat** for anything structured. The
enumeration tool below writes a file for exactly this reason.

---

## 1. Phase 0.1 — Encoder and capture capabilities (env S)

```
probe-windows                  # NvFBC via ToCuda, the GPU-resident path
probe-windows --tosys          # also the sysmem path, which copies every frame
probe-windows --enable-nvfbc   # NvFBC_Enable: needs elevation, resets the driver
```

**Run the capture section with something animating full-screen**, and once with
Windows HDR off and once on. An idle desktop has produced two wrong conclusions
in this investigation already; the probe now refuses to draw one instead.

Run `tools/probe-windows`. It answers more than the roadmap asked for, because
the extra questions cost nothing once the encoder session is open and each one
de-risks Phase 3.

**Measured on env S.** This project was originally specified against an RTX 5070
and the hardware is actually a 4070, so these are the numbers that count and
CLAUDE.md has been corrected to match. Nothing in the matrix below was affected
by the mix-up — both parts are single-NVENC consumer cards with an AV1 encoder,
which is all Phase 0.1 asks about — but the Blackwell-only items are now simply
absent rather than deferred: no 4:2:2, no MV-HEVC, and Split Frame Encoding was
never reachable with one encoder either way.

- [x] **Driver is new enough.** The probe reports **NVENC API 13.1**, exactly
      what this build targets, and the startup gate compares against that rather
      than a driver branch number. Worth keeping straight: AV1 encode arrived
      with Ada and needs only SDK 12.0+, so 13.1 is the floor for *the headers
      this build is written against*, not for AV1. The probe prints the API
      version and not the driver version string; if the driver number itself is
      ever wanted, that is a line to add to the probe rather than a fact to infer
      from this.
- [x] **NvFBC unlocks. Proven, with the A/B that makes it evidence.** The first
      answer — "no `NvFBCCreateInstance`, treat as unavailable" — was a **false
      negative from the wrong entry point**. There are two generations:
      `NvFBCCreateInstance` is the 7.x/Linux API, and the one this driver
      exports is the legacy Windows set. A missing modern export is a *version
      detection*, not a verdict.

      Legacy NvFBC is gated to professional cards by a private-data key, so the
      probe asks unkeyed and keyed and prints the pair:

      ```
      CreateEx unkeyed: ERROR_DRIVER_FAILURE -- object 0, max 0x0
      CreateEx keyed  : NVFBC_SUCCESS        -- object 1, max 3840x2160
      ```

      A keyed-only run would prove nothing — a Quadro passes either way. The
      unkeyed failure beside it is what makes the key the cause.

      **Two traps for anyone re-testing.** `GetStatusEx` reported
      `capture_possible 1` *both ways*: status is not the gate, `CreateEx` is, so
      a status-only check gives the wrong answer. And the probe's original
      `multi_client` label was wrong — at `NVFBC_DLL_VERSION 0x70` that bit is
      `bSupportConfigurableDiffMap`, with `bSupportImageClassification` new at
      bit 5. Same struct size, so nothing errored; it simply printed a real bit
      under a stale name.
- [x] **Capture works, via the V2 setup struct — not the V3 the headers describe.**
      Setup with `NVFBC_TOSYS_SETUP_PARAMS_V3` returns `ERROR_INVALID_PTR` every
      time; the 0x50-era **V2** layout is accepted and captures 3840×2160 with no
      failed grabs.

      Both structs are **504 bytes**, so the size check in the version word
      passes either way and only the version nibble and field order differ — the
      driver reads `ppBuffer` from offset 16, not 24. That is why the failure was
      `INVALID_PTR` rather than a version error, and why it survived four rounds
      of inspection: the parameters matched NVIDIA's own sample field for field,
      and the layout was confirmed against the MSVC ABI with clang. Both were
      right; the *generation* was wrong.

      The vtable dump settled the other half — all five slots resolve inside
      `NvFBC64.dll`, so the call was reaching NvFBC and the driver really was
      refusing. Worth keeping: the driver reports `0x70` from `GetSDKVersion`
      while rejecting that generation's setup struct, so **the reported version
      does not tell you which struct generation to send.**
- [~] **Capture throughput vs DDA — measured in SDR, undetermined in HDR.**

      | path | delivery rate | per-grab cost |
      |---|---|---|
      | DDA (SDR) | **111.0 new frames/sec** | ~0 to poll |
      | NvFBC ToSys, blocking | **75.1 unique frames/sec** | 7.20ms p50 / 21.06ms p99 |
      | NvFBC ToSys, polling | 70.4 unique frames/sec | 3.61ms p50 / 4.21ms p99 |

      **0.68x DDA in SDR**, at 4K with motion on screen, and a 21ms p99 tail
      against a 16.7ms frame.

      **That verdict does not survive turning HDR on**, which is the actual
      workload. The next run showed ARGB10 taking 174 unique frames in a shorter
      window — roughly 96 unique/sec against DDA's 80.4 — so NvFBC may well win
      in the mode that matters. It is not a result yet: the two figures came from
      different windows with no control beside the ARGB10 run. The probe now
      measures **both formats with their own DDA control**, and the verdict waits
      for that.

      Three earlier attempts at this number were wrong, recorded so the mistakes
      are not repeated: the first compared against an *idle* desktop; the second
      polled with `NOWAIT`, where 447 of 500 grabs re-copied a frame already seen
      at full cost; the third drew an SDR conclusion about an HDR workload. Only
      a blocking figure against a same-window control is comparable, because
      DDA's `AcquireNextFrame` returns free when nothing is ready — approaching a
      million attempts per window — while ToSys pays a full copy every time.

      **Superseded as the thing to measure.** ToSys is ruled out by its *shape*,
      not its rate — see the readback row below — so the comparison that decides
      Phase 0.1 is the ToCuda one.
- [x] **`NvFBCToCuda` — measured, controlled, and it does not beat DDA.**
      Four same-window comparisons, both colour modes, both pixel formats:

      | desktop | format | NvFBC blocking | DDA control | ratio |
      |---|---|---|---|---|
      | HDR on | ARGB | 37.3 /s | 39.5 /s | 0.94× |
      | HDR on | ARGB10 | 33.0 /s | 38.5 /s | 0.86× |
      | HDR off | ARGB | 34.2 /s | 39.9 /s | 0.86× |
      | HDR off | ARGB10 | 34.1 /s | 38.3 /s | 0.89× |

      **Per-grab 0.15–0.19ms p50**, against ToSys's 3.6ms — a twentyfold
      difference that confirms the frame really is staying on the GPU, and the
      sanity check this run was designed around. GPU residency works exactly as
      advertised. It just buys nothing.

      **Read the DDA column before the ratio.** DDA itself only managed ~38–40
      frames/sec on a **144Hz** display, so the content was producing about 40fps
      and *neither path was stressed*. What these numbers establish is that NvFBC
      sees nothing DDA misses; they do not establish behaviour under content that
      outruns the refresh rate.

      That gap is the honest limit of this experiment, and the next chunk closes
      it: content rendering *above* 144Hz is what a throughput answer would need,
      and **present→capture latency is measurable in software** — no camera. A
      borderless window flipping a corner region on every `Present`, with each
      backend timing when it sees the flip, gives both. That interval is where
      DWM composition sits, so the same harness finally prices the budget's
      largest line.

      Setup notes worth keeping: the interface id is **0x1007** at 0x70 where the
      older header says `0x1006`; `NvFBCCudaSetup` is **vtable slot 1** (slot 0 is
      `GetMaxBufferSize`, which is how you learn what to allocate); `bHDRRequest`
      is bit 1, read from the header. **Teardown order is load-bearing** — free
      the device buffer *before* releasing the session, and never call
      `cuCtxDestroy` on the context NvFBC created for you. Doing the latter took
      the probe out mid-run.

      `nvcuda.dll` costs no build dependency, but its exports carry `_v2`
      suffixes: `cuInit` resolved bare, everything else via `_v2`. A bare-name
      loader would have reported CUDA missing.
- [x] **10-bit and HDR capture works.** With Windows HDR on, `NVFBC_TOSYS_ARGB10`
      captures and **`bIsHDR` comes back set**. The inferred V2 bit position for
      `bHDRRequest` was therefore right — bit 3, the same slot it occupies in the
      V3 struct this driver rejects.

      The buffer is **A2B10G10R10**, a 10-bit integer format — *not* the scRGB
      linear FP16 CLAUDE.md's HDR notes assume. If this path is ever used, the
      shader's input stage changes: no 80-nit normalise from linear, and the
      transfer and primaries of that buffer still need establishing.
- [x] **In HDR mode, ToSys's 8-bit ARGB capture silently freezes.** One unique
      frame across 800 grabs while DDA saw 80.4 new frames/sec over the same
      window — so the screen was moving and the capture path was not. It does not
      fail, it does not error, it returns one stale frame forever.

      **This is `NVFBC_TO_SYS`-specific, and the first record of it here was too
      general.** ToCuda's 8-bit path captures an HDR desktop perfectly well —
      37.3 unique frames/sec against DDA's 39.5, 287 unique frames in 300 grabs.
      The lesson survives the correction, because the failure mode is the point
      rather than the interface:

      This is the same shape as the av1C trap in CLAUDE.md: configures cleanly,
      produces nothing usable. Any capture backend must pick its pixel format
      from the desktop's colour mode, and **must re-pick when that mode changes**
      — Windows 10's HDR toggle is global and flips under a running session. The
      probe now names this case explicitly rather than reporting it as a static
      desktop.
- [ ] **`--enable-nvfbc`, only if `CreateEx` refuses while status says capture is
      possible.** `NvFBC_Enable` needs elevation and **resets the display driver**,
      which on a box someone is watching is indistinguishable from a crash — so
      the probe never calls it unless asked by name. Not needed so far: `CreateEx`
      succeeded without it.
- [x] **HEVC caps.** Reference invalidation, intra refresh, subframe readback,
      10-bit and dynamic bitrate change all supported.
- [x] **AV1 caps — parity holds on everything load-bearing.** Reference
      invalidation *and* subframe readback are both supported. This was the most
      consequential question in Phase 0 and it came back the good way.
- [ ] **Second NVENC session detected** when ShadowPlay, Instant Replay or OBS is
      running. The driver time-shares one physical encoder and per-frame times get
      jittery in a way that reads exactly like our bug, so this becomes a startup
      warning. **Half-done:** the NVML query works and reports `0 sessions, 0 fps,
      0 us` on an idle box, so the plumbing is proven. It has never been seen
      report a *non-zero* count, which is the half that matters — re-run with OBS
      open before trusting the warning.

| Cap | HEVC | AV1 |
|---|---|---|
| Ref pic invalidation | yes | **yes** |
| Intra refresh | yes | yes |
| Subframe readback | yes | **yes** |
| 10-bit encode | yes | yes |
| Dynamic bitrate change | yes | yes |
| YUV 4:4:4 | yes | no |
| Max width × height | 8192 × 8192 | 8192 × 8192 |
| Max level | 186 → 6.2, the HEVC maximum | 23 `seq_level_idx`, far above 4K60 |
| Encoder engines | 1 | 1 |

Max bitrate is absent from the table because there is no per-codec cap to query:
the real ceiling is the Shield's decoder, well below 1GbE. CLAUDE.md's ~150 Mbps
practical limit stands as the operative number.

### What 0.1 settled

**AV1 keeps reference invalidation, so Phase 4 is unchanged.** ROADMAP 0.4 spelt
out the bad branch: the Homatics has no HEVC path behind it, so an AV1 encoder
without `SUPPORT_REF_PIC_INVALIDATION` would have degraded NACK recovery on that
box to `RequestIdr` and changed Phase 4's design. It does not. Both codecs keep
the same recovery strategy — still **two** state machines, per CLAUDE.md, because
AV1's 8-slot explicit signalling differs enough that sharing one would be the bug.

**AV1 keeps subframe readback, so tiles can be emitted as they complete.** With
one NVENC and no SFE this is the only mechanism that hides encode time, and it
exists on both codecs rather than just HEVC.

**One encoder engine, as AD104 has.** No Split Frame Encoding — it needs two or
more. Encode time stays a fixed 5–10ms floor, which is what makes the line above
load-bearing rather than an optimisation. Note the floor itself is still the
inherited Blackwell-era *estimate*; §4 is where Ada's real number goes.

**NvFBC is unlocked, working, GPU-resident, and kept — as an opt-in backend
beside DDA and WGC, not as a default.**

Across every controlled comparison `NvFBCToCuda` delivered **0.86–0.94× Desktop
Duplication**, never above it. The GPU-resident path works exactly as intended —
0.15–0.19ms per grab against ToSys's 3.6ms — so this is not a badly built test.

**But that settles throughput, which is not what this project optimises**, and
the first write-up of it here ("closed, does not beat DDA") was broader than the
evidence. Three things it does not cover:

- **Latency was never measured.** Equal delivery at lower per-frame cost is still
  a win, and DDA's `AcquireNextFrame`-plus-copy has no number beside NvFBC's
  0.15ms.
- **Neither path was stressed.** DDA itself sat at ~38–40 frames/sec on a 144Hz
  display in every run — the content was the limit, not the capture path.
- **DDA has failure modes NvFBC may not share**: black on DRM-protected content,
  dead on the secure desktop, constant `AccessLost`. A second GPU-resident path
  is resilience as much as speed.

So the ~16.7ms DWM line still stands unmeasured, and NvFBC does not remove it —
that much *was* established. What decides NvFBC's real worth is the
**present→capture latency harness** that follows Phase 0.

**Why it is opt-in and never the default.** Three reasons, all independent of any
measurement, and all still true now that it is being kept:

- The key is undocumented and can stop working on any driver update. A capture
  backend that can vanish in a driver release is not a default; at most it is an
  opt-in fast path behind one that always works.
- NVIDIA deprecated NvFBC on Windows, and states the **last supported Windows 10
  version is 1803, build 17134**. CLAUDE.md's floor is 1903+, so the *entire*
  supported OS range is past it. Deprecated is not removed — it demonstrably
  works here — but nothing obliges it to keep doing so.
  *(An earlier draft of this file said "from the October 2019 update", taken from
  a search snippet rather than NVIDIA's own wording. 1803 is the real number.)*
- Phase 3 still needs DDA and WGC regardless, because they are what works on an
  unpatched machine.

  What it *would* license is a **measurement**: how much of the ~16.7ms does
  skipping composition actually recover on this hardware? That number is worth
  having even if the backend never ships, because it prices the swapchain-hook
  work in Phase 7 — which is the other way past composition and the expensive
  one. Getting a real figure cheaply, before committing to hooking, is the whole
  value of this result.

If it comes back negative even with the key, the original conclusion stands:
delete the backend, and swapchain hooking becomes the only remaining route to a
pre-composition frame — which does not promote it either, given it is
per-process, a poor fit for Big Picture launching each game into a new window,
and carries an anti-cheat warning.

**4:4:4 on AV1 is absent and irrelevant.** Android decoders want 4:2:0, as
CLAUDE.md says. Recorded only so its absence is never mistaken for a finding.

---

## 2. Phase 0.2 — Decoder enumeration (env A and B)

```bash
./gradlew -p android assembleDebug
adb install -r android/app/build/outputs/apk/debug/app-debug.apk
adb shell am start -n com.trexx.sunburst/.probe.DecoderProbeActivity
adb pull /sdcard/Android/data/com.trexx.sunburst/files/decoders.json
```

- [x] **Every decoder enumerated, not just the first match.** Errata #15 in
      `decoder-errata.txt`: some devices do not support `FEATURE_LowLatency` on
      their first compatible decoder, and picking the first one silently loses it.
- [x] **Shield: HEVC Main10 at 4K60 is present**, and there is no AV1 decoder at
      all — as CLAUDE.md says. *(Verified on SHIELD Android TV, `mdarcy`,
      Android 11 / API 30.)*
- [x] **Homatics: AV1 Main10 at 4K60 is present.** `c2.amlogic.av1.decoder`,
      hardware, `FEATURE_LowLatency` **true**, `supports4k60` **true**, 9
      instances. Profiles cover Main8, **Main10**, Main10HDR10 and
      Main10HDR10Plus at **level 5.1** — which is what 4K60 needs. The box's only
      path exists and looks right. *(Verified on SEI Robotics Box R 4K Plus,
      `YYJ`, Amlogic, Android 14 / API 34, `armeabi-v7a`.)*
- [x] **Vendor low-latency keys** on the Amlogic: **none survived**, on any
      decoder. Read that as *no evidence*, not *unsupported* — the probe's own
      documentation says so, because a key can take effect without appearing in
      `getInputFormat()`. Errata #16/#17 can only be settled by decoding a real
      stream and seeing whether frames come out, which is Phase 5.

      `FEATURE_LowLatency` is **true** on the AV1 decoder, so the documented
      mechanism is available and the undocumented ones may not be needed at all.

### Shield results (verified)

| Decoder | MIME | HW | FEATURE_LowLatency | KEY_LOW_LATENCY | 4K60 | Max res |
|---|---|---|---|---|---|---|
| `OMX.Nvidia.h265.decode` | hevc | yes | **yes** | silent | **yes** | 3840×2176 |
| `OMX.Nvidia.h265.decode.secure` | hevc | yes | no | silent | yes | 3840×2176 |
| `OMX.google.hevc.decoder` | hevc | no | no | rejected | no | 4096×4096 |

`OMX.Nvidia.h265.decode` is the target. Profile list includes `4096`
(`HEVCProfileMain10`). The software decoder cannot do 4K60 and is not a fallback.

### Homatics results (verified)

| Decoder | MIME | HW | FEATURE_LowLatency | 4K60 | Max res | Inst |
|---|---|---|---|---|---|---|
| `c2.amlogic.av1.decoder` | av01 | yes | **yes** | **yes** | 3840×3840 | 9 |
| `c2.amlogic.av1.decoder.secure` | av01 | yes | yes | yes | 3840×3840 | 2 |
| `c2.amlogic.hevc.decoder` | hevc | yes | yes | yes | 4096×4096 | 9 |
| `c2.amlogic.hevc.decoder.secure` | hevc | yes | no | yes | 4096×4096 | 2 |
| `c2.android.av1.decoder` | av01 | no | no | no | 1280×1280 | 32 |
| `c2.android.hevc.decoder` | hevc | no | no | no | 2048×2048 | 32 |
| `OMX.google.hevc.decoder` | hevc | no | no | no | 2048×2048 | 32 |

`c2.amlogic.av1.decoder` is the target. AV1 `profileLevels` decode to Main8,
Main10, Main10HDR10 and Main10HDR10Plus, all at level 5.1. **Max width is 3840,
not 4096** — exactly 4K and not a pixel more, so anything that rounds a width up
will fail on this box and not on the Shield.

Neither software decoder reaches 4K, so there is no fallback behind the Amlogic
one. `maxInstances` 9 is ample for a single session.

### Quirks seeded from this, and what enumeration cannot seed

CLAUDE.md's `DecoderQuirks` is keyed on `getName()` + `Build.MODEL`, so the
Homatics entry keys on `c2.amlogic.av1.decoder` + `Box R 4K Plus`. Enumeration
fills in almost none of the fields, and it is worth being explicit about which:

| Field | Value | Basis |
|---|---|---|
| `ref_invalidation` | **false** | Not observable by enumeration. CLAUDE.md: Amlogic decoders are known to mishandle it. Default off until a real stream says otherwise. |
| `intra_refresh` | **false** | As above, same trap, same default. |
| `slice_output` | unknown | Needs a decode test; `FEATURE_LowLatency` is not the same question. |
| `needs_annexb_startcodes` | n/a for AV1 | AV1 is OBUs; the field exists for the HEVC path this box does not use. |
| `max_bitrate_hint` | unset | No enumeration source. Phase 0.3 would bound it from the link — and 0.3 is not answered. |

So the table is **seeded with decoder identity and capability, not with quirks**.
The quirks themselves land in Phase 5, when there is a stream to feed it.

> **The HEVC rows are a trap, and enumeration cannot see it.**
> `c2.amlogic.hevc.decoder` reports hardware, `FEATURE_LowLatency` true and 4K60
> true — it looks *better* on paper than the AV1 decoder, and ROADMAP 0.4 struck
> it permanently because its low-latency path is known broken in practice.
> Enumeration is not capable of detecting that. Anyone reading this table without
> reading that decision would pick HEVC for this box.

**Read `KEY_LOW_LATENCY` as three-state, not a boolean.** `silent` means
configure accepted the key without echoing it back, which proves nothing either
way: MediaCodec silently ignores keys it does not recognise, and a key that took
effect may still not appear in `getInputFormat()`. Only `rejected` — configure
threw — is an unambiguous answer. `FEATURE_LowLatency` remains the thing to trust.

> Both flaws above were found by running the probe rather than by reading it. The
> first version treated a silently-ignored key as accepted, which made every
> decoder on the Shield report all five vendor low-latency keys as supported —
> including Google's software decoder, which has never heard of any of them.

---

## 3. Phase 0.3 — Network PHY (env A and B)

```bash
tools/check-phy.sh [adb-serial]
```

- [x] **Shield: link is gigabit.** 216 Mbps measured, well past a 100Mbit PHY's
      ~94 Mbps ceiling. *(Verified over adb-over-TCP.)*
- [x] **Homatics: closed. The 100Mbit failure mode is ruled out.** `eth0` exists,
      the Ethernet service is enabled, and **the gigabit port is present and
      unused** — the box is currently on `wlan0`: SSID "House LANister", 5240 MHz,
      Wi-Fi 6, RSSI −61, 1200 Mbps PHY rate.

      Measured 494 / 119 / 136 Mbps across three attempts. **Be clear what that
      does and does not establish.** It is a wi-fi number, and the script's own
      warning fired saying so, so it is *not* a measurement of the Ethernet PHY.
      What it does settle is the thing 0.3 was actually worried about: this box's
      usable throughput is not capped near 80 Mbps, so AV1's efficiency is not
      load-bearing and Phase 4 is not being designed around a 100Mbit ceiling.

      Closed on that basis plus a known-gigabit port, not on a wired measurement.
      The 4× spread across attempts is wi-fi airtime, not a link problem.
- [ ] Confirm the switch port agrees, not just the box — a single bad pair
      negotiates 100 and looks exactly like a hardware limit. **Deferred to
      whenever the box is actually wired**, since it cannot be checked before
      there is a cable in it. Re-run `tools/check-phy.sh` then: it will stop
      warning about the missing ethernet interface, which is itself the signal
      that the number finally means what it says.
- [ ] **If the Homatics is meant to run on Wi-Fi in production, that is a
      different and worse question than 0.3.** 5GHz Wi-Fi 6 at RSSI −61 has the
      *bandwidth* for a 70–100 Mbps AV1 stream. What it does not have is bounded
      delivery: this project's rate controller works to a frame deadline, and
      airtime contention produces exactly the jitter a deadline cannot absorb.
      CLAUDE.md assumes wired for both clients. Worth settling deliberately
      rather than by whatever happens to be plugged in.

**Confirmed again on the Homatics:** `/sys/class/net/eth0/{speed,carrier,operstate}`
are all `Permission denied` there too, so the link state cannot be read even to
distinguish "no cable" from "cable, link down".

**The obvious check does not work, and the obvious substitute lies.**
`/sys/class/net/eth0/speed` is `Permission denied` for the shell user under
SELinux, as are `ip link` and `ethtool` (verified on the Shield). `dumpsys
ethernet` does report `LinkUpBandwidth>=100000Kbps`, but that is Android's
hardcoded default for the Ethernet transport, not a measured rate — it reads as
exactly the failure being looked for, on a link that is actually gigabit. The
script measures throughput instead, which makes it a reliable yes/no and not a
rate meter: adb tops out around 200 Mbps.

| Device | Measured | Verdict |
|---|---|---|
| Shield | 216 Mbps | gigabit, wired |
| Homatics | 494 Mbps peak, over **Wi-Fi** | gigabit port present, unused; 100Mbit ceiling ruled out |

---

## 4. Instrumentation on the real hardware (env S, A, B)

`record()` measures **29.3 ns** on the Linux development machine, where the clock
is `clock_gettime(CLOCK_MONOTONIC)` through the vDSO and the read is 20.7 ns of
that total. Windows uses `QueryPerformanceCounter` and the Android boxes are much
slower cores, so the budget needs re-confirming on each.

- [ ] **Windows: `sunburst-instr selftest`** runs and reports plausible per-stage
      numbers. Then `cargo bench --bench instr` for the real `record` cost — the
      50 ns budget is a claim about the target, not about the dev machine.
- [ ] **Shield and Homatics**: same, once the client exists. The Homatics'
      little cores are the worst case in the system.
- [ ] **No dropped samples** under a sustained 4K60 run. Drops mean the drain
      thread is not keeping up, and every percentile becomes unquotable.

| Env | record() p50 | clock read | Dropped over 5 min |
|---|---|---|---|
| Linux dev | 29.3 ns | 20.7 ns | 0 |
| S (Windows) | | | |
| A (Shield) | | | |
| B (Homatics) | | | |

---

## 5. Management UI (env S)

The API, pairing, config and catalogue are all covered by tests on the
development machine — 130 of them, against a fake `Host`. What follows is only
the part that a fake cannot answer.

- [ ] **Autostart survives a logoff and a fast user switch.** The scheduled task
      is `ONLOGON`, so this is the whole point of it. Log out, log back in,
      confirm the server is up and the UI answers.
- [ ] **The task really does run with highest privileges.** Check the elevated
      flag on the Status panel. If it reads "no", `SendInput` cannot reach an
      elevated window past UIPI, and that presents as input doing nothing in one
      specific game rather than as an error.
- [ ] **Creating the task without elevation fails visibly.** `schtasks /RL
      HIGHEST` needs an elevated caller; confirm the UI reports the failure
      rather than silently leaving autostart off.
- [ ] **Restart hands the port over.** The replacement retries its bind for ten
      seconds while the old process exits. Confirm the UI comes back rather than
      the new process dying on `EADDRINUSE`.
- [ ] **A real game launches**, with prep commands run before and undone after.
      Use something that actually changes display state, so a missed undo is
      visible.
- [ ] **Prep undo after a crash, not a clean exit.** Kill the game with Task
      Manager and confirm the undo commands still run — this is the path that
      leaves a display in the wrong mode.
- [ ] **`steam://open/bigpicture` launches** through `ShellExecute`. It has no
      child process to wait on, so confirm the UI does not report it as running
      forever.
- [ ] **A device pairs from the TV and survives a restart.** The PIN is shown on
      the client and typed into the browser. Then restart the server and confirm
      it is still paired — a pairing that lives only in memory is the failure to
      look for.
- [ ] **Revoking really revokes.** Remove a client, restart, and confirm it
      cannot reconnect without pairing again.

---

## 6. Keyboard and mouse injection (env S)

The decisions are unit-tested on the development machine — 18 of them, covering
extended keys, modifier reconciliation and the `MOUSEEVENTF` mapping. What
follows is everything those tests structurally cannot reach.

Pair once, then drive it from the box itself:

```
fakeclient pair  --server 127.0.0.1:47811
fakeclient input --server 127.0.0.1:47811 --script keyboard
fakeclient input --server 127.0.0.1:47811 --script mouse
```

- [ ] **Keys reach a game, not just Notepad.** The scancode-versus-virtual-key
      trap only shows up in something reading DirectInput or raw input. A VK
      based `SendInput` types fine into Notepad and does nothing in a game, so
      Notepad working proves nothing at all.
- [ ] **The extended set produces the right key**, not its numpad twin: arrows,
      Ins/Del/Home/End/PgUp/PgDn, right Ctrl, right Alt, numpad Enter, numpad
      divide. Without the flag Home becomes 7 and Up becomes 8. Check against
      `keymap::EXPECTED_EXTENDED`, which is the checklist, not the mechanism.
- [ ] **Ctrl+Shift+Esc opens Task Manager.** Three keys, the worst case for
      modifier ordering, and it needs elevation to work at all.
- [ ] **Alt+F4, Ctrl+V, Win, Win+D** behave. Each exercises a different modifier
      path.
- [ ] **No stuck modifier** after releasing a chord. Then the harder one: kill
      `fakeclient` mid-chord and confirm nothing is left held. The injector
      releases what it believes is down when the queue closes.
- [ ] **Input survives a UAC prompt**, and a lock/unlock cycle. This is the
      desktop re-attach, and the item Phase 2's acceptance names. Expect input to
      do nothing *while* the secure desktop is up — that is correct, and capture
      cannot see it either — and to resume afterwards without a restart.
- [ ] **Input reaches an elevated window**, or the Status panel honestly says
      the server is not elevated. UIPI, and it presents as "input does nothing in
      one specific game".
- [ ] **Absolute mode lands where it should on a multi-monitor setup**, and on a
      scaled display. `MOUSEEVENTF_VIRTUALDESK` is what makes 0–65535 span every
      monitor rather than the primary; without it a second display is
      unreachable, and DPI scaling is where the coordinates go wrong.
- [ ] **Both X buttons do different things.** They share `XDOWN`/`XUP` and are
      told apart only by `mouseData`, so getting it wrong turns X2 into X1.
- [ ] **Wheel and horizontal wheel scroll the right way.** The sign is the
      direction.
- [ ] **Enhanced Pointer Precision off.** Note how relative motion felt with it
      on before turning it off, so the decision not to compensate is recorded
      against evidence rather than assumption.

`Injector::stats()` carries the counters worth reading when something does
nothing: `dropped` (queue full), `refused` (`SendInput` rejected it — usually
UIPI), `reattached` and `attach_failed`.

---

## 7. Present→capture latency (env S)

```bash
probe-windows --latency
```

**The one measurement that prices the budget's largest line.** CLAUDE.md gives DWM
composition ~16.7ms — more than encode — and nothing has ever measured it. Every
capture number before this was throughput, which is not what this project
optimises and which answered none of it.

A camera is *not* needed for this. Glass-to-glass needs one, because it spans the
decoder and the panel; present→capture does not, and that interval is where
composition sits. Both ends are QPC timestamps taken in one process.

**Method.** A topmost 256×256 window at (64, 64) presents continuously and changes
colour every 100ms, publishing the QPC of the `Present` that carried each change.
Each backend — DDA, WGC, NvFBC ToCuda — polls the desktop pixel at the window's
centre and times the change against that timestamp. Backends run **one at a
time**; concurrent sessions would measure contention between them.

Three details that are decisions, not incidentals:

- **The read point is inset from (0, 0).** WGC draws a capture-indicator border
  around the captured region on some Windows versions, and at the origin it would
  land exactly where the probe reads — timing the border instead of the content.
- **Change is detected on raw bytes, not decoded colour.** The three paths return
  `B8G8R8A8`, `R16G16B16A16Float` and `A2B10G10R10` respectively; comparing bytes
  needs no per-format branch, and the presenter's timestamp is what carries the
  meaning.
- **Flips are 100ms apart on purpose.** A colour alternating every frame cannot be
  attributed — a refresh-capped backend has no way to say which `Present` it is
  looking at. A `flip_seq` guard stops one flip yielding several samples.

- [x] **Run with HDR off and on.** Done. The colour mode makes no material
      difference to any of the three — see the table.
- [x] **Sanity check passed.** DDA and WGC p50s land within 0.4ms of each other
      and swap places between runs, which is what two post-composition paths
      should look like. The harness is measuring the path, not itself.
- [x] **Tearing was available**, so `Present` really was unbound: the presenter
      issued 14,000–16,000 presents/sec.

### Results (env S, 144Hz display)

| Backend | p50 | max | stress: distinct frames/sec |
|---|---|---|---|
| DDA | **3.83ms** / 4.41ms HDR | 7.46ms | 125.0 / 103.5 HDR |
| WGC | **4.24ms** / 4.24ms HDR | 7.84ms | 127.2 / 132.5 HDR |
| NvFBC ToCuda | **5.28ms** / 5.32ms HDR | 8.56ms | 123.0 / 131.0 HDR |

*(The p99 column is gone from this table on purpose. Eighty samples cannot
resolve a 99th percentile — with `n ≤ 100` the index lands on the last element,
so the harness was printing the maximum twice under two names. The flip interval
is now 25ms rather than 100ms, giving ~320 samples, and the report says so when
the count is too low to mean anything.)*

### What this says

**CLAUDE.md's ~16.7ms DWM composition line is wrong, and wrong in an
instructive way.** Measured present→capture is **~4ms typical, ~7.5ms worst** on
this machine.

The shape of the numbers says why. This display refreshes at 144Hz — a 6.94ms
period. A capture path that waits for the next composition pass would show
latency spread roughly uniformly across that interval: mean **3.47ms**, max
**6.94ms** plus whatever the capture itself costs. Measured: p50 3.83ms, max
7.46ms. That fits closely enough that the model is worth stating —
**composition latency is about half the desktop's refresh interval, not a fixed
frame time.**

Which makes 16.7ms the *worst case at 60Hz*, recorded as though it were the
typical cost at any refresh rate. At 60Hz the same model predicts ~8.3ms typical
and ~16.7ms worst, so the original figure was not invented — it was the maximum,
mislabelled, at a refresh rate this machine does not run.

**Nothing escapes composition.** With tearing on and the presenter issuing
~15,000 presents/sec, all three backends delivered **123–133 distinct
frames/sec** — the refresh rate, give or take. Not 1%, not 200: the panel's
cadence. NvFBC included, which finally closes that question with the stress
condition the throughput runs never managed to create.

**NvFBC loses on latency too**, by 1.2–1.5ms consistently, in both colour modes.
It was retained specifically because latency was unmeasured and might have
favoured it. It does not. What is left is the resilience argument alone — DDA goes
black on DRM-protected content and dies on the secure desktop — which is real but
much narrower than the case it was kept on.

**DDA and WGC are equivalent.** They differ by less than half a millisecond and
change places between runs, so the Phase 3 selection rule (WGC on Win11, DDA on
Win10) stands on compatibility grounds, with nothing to choose on latency.

**One new and actionable finding: run the server's desktop at a high refresh
rate.** If composition costs half a refresh interval, then 144Hz costs ~3.5ms
where 60Hz costs ~8.3ms. That is ~5ms of glass-to-glass for a display setting,
independent of anything in this codebase, and it is worth more than several of
the optimisations the roadmap has planned.

**The answer came back "a few milliseconds", so the second branch is taken.** The
largest line in the budget shrinks by roughly 4×, and Phase 7's swapchain hook
loses most of its reason to exist: it is per-process, a poor fit for Big Picture
launching each game into a new window, and carries an anti-cheat warning — for
about 4ms at 144Hz. That trade was defensible against 16.7ms. It is not
defensible against 4ms.

---

## 8. Phase 2's pad path — two candidates (env S)

Phase 2's acceptance needs a gamepad in a real game, and the roadmap's
`vigem-client` line predates some news: **ViGEmBus was retired and archived on
2 November 2023** after a trademark conflict with ViGEM GmbH. It still works and
still ships EV-signed, and a frozen ABI is the easiest thing to hand-write a
binding against — but it is worth knowing what else exists before committing.

### What was surveyed

| Option | Finding |
|---|---|
| **ViGEmBus** | Archived Nov 2023, BSD-3, 4.2k stars, EV-signed, works. The fallback: ~6 ioctls, ABI frozen by abandonment. X360 target caps motion/trigger-rumble/battery; its **DS4 target** (`IOCTL_DS4_SUBMIT_REPORT`, `0x2AA80C`) carries gyro if that ceiling ever matters. |
| **HIDMaestro** | MIT, active, created **2026-04-10**. Reaches DirectInput, XInput, SDL3 **and WGI/GameInput**, byte-exact VID/PID. **User mode, self-signed cert, no EV, no test-signing.** |
| **USB/IP** (`usbip-win2`) | **Attestation signed** — installs normally. Active: releases Apr/Jul/Sep 2026. v0.9.8.0 added WSK event callbacks aimed at *"devices that generate small amounts of data but at a high frequency, such as HID keyboard/mouse"*. Use **≥ 0.9.8.0**; 0.9.7.8 shipped a memory-corruption BSOD. |
| `libvirtualhid` | **Ruled out on licence.** Its Windows driver, broker and generated MSI are under the "LizardByte Source-Available License 1.0", which opens *"This License is not an open source license"* and whose §3(b) forbids distribution "by any means… commercial, non-commercial, educational, individual, charitable, internal, public, private, or otherwise". Sunburst is GPL-2.0-or-later in a public repo, so vendoring is distribution and distribution is prohibited. The MIT half does not help: on Windows the driver *is* the functionality. |
| WinUHid | MIT, from Moonlight's author, but **dormant since 2025-05-28** and framework-level — you supply the HID descriptor, so generic HID rather than XUSB. |
| `inputtino` | Linux `uhid`. Not applicable to a Windows server. |

> **A correction recorded deliberately.** USB/IP was first written off here because
> the Windows client was said to require Test Signing Mode. **That was wrong** —
> it has been attestation signed since v0.9.7.7 (2026-04-21). The claim came from
> a search summary citing an old issue rather than from the project's own
> releases, and it was then used to argue against an explicit instruction to
> evaluate USB/IP first. Kept visible because the failure was the method, not the
> conclusion: release notes were available and were not read.

### Investigation A — USB/IP

The prize is Phase 8: forward `045e:02e6` and let Windows' own driver own the
adapter, deleting the vendored MT7612U radio (~3,460 lines) and the ~6,100 lines
of GIP/controller/wired, and lifting the X360 ceiling. The cost is TCP where an
interrupt transfer becomes a round trip.

**The decisive unknown is Android, not Windows.** A USB/IP *server* normally needs
the `usbip-host` kernel module and root; stock Android TV has neither, so an
unrooted client would mean implementing the USB/IP server protocol over
`UsbDeviceConnection` ourselves. So test the Windows half first, with the **Linux
dev machine standing in as the server** — no Android code written until the
numbers justify it.

**The Linux side is not ready yet.** `usbip` and `usbipd` are installed, but
`usbip_host` cannot load: the module directory for the running kernel
(`7.1.9-200.fc44`) is empty while the installed `kernel-modules-extra` is
`7.2.5-200.fc44`. **Reboot into the newer kernel** so the two match. And the
device to forward has to be plugged in *here* — this machine currently has only a
webcam on its bus. On Windows: `usbip-win2` ≥ 0.9.8.0.

**The C++ SDK in `../include/usbip` is not needed, and could not be used as it
stands.** Its signatures pass `std::string`, `std::vector` and `std::optional`
across the ABI — `std::optional<std::vector<imported_device>>
get_imported_devices(HANDLE)` — so it is not callable from Rust without a C++
shim, and this cross-build contains no C++ at all. The installer ships
`usbip.exe`, so attaching is a command. The SDK only earns that cost if the
*server* ever automates attach, which is Phase 8 work rather than a measurement.

- [!] **Attach gate: fails for the Xbox controller, in both receive modes.**
      Exported a wired Xbox One Controller (`045e:02dd`, firmware 2015, busid
      `1-1`) from the Linux box on kernel 7.2.5 with `usbip_host`, attached from
      Windows with `usbip-win2` 0.9.8.0. Same result with `low_latency` and with
      the default `zero_copy`. The kernel log on the Linux side, both times:

      ```
      usbip-host 1-1: stub up                                 <- Windows attached
      usbip-host 1-1: usb_clear_halt done: devnum N endp 1    <- x11 over ~8s
      usbip-host 1-1: USB disconnect, device number N         <- device RESET
      usb 1-1: new full-speed USB device number N+1
      input: Microsoft X-Box One pad (Firmware 2015)          <- xpad reclaims it
      ```

      Windows' Xbox driver clears the halt on **endpoint 1 — the interrupt pipe
      that carries GIP** — about once a second, which is a host repeatedly
      resetting a pipe whose transfers keep failing. After ~8s it escalates to a
      device reset, USB/IP relays that to the physical pad, the pad re-enumerates
      on Linux, `xpad` binds it, the export is gone, and Windows sees an unplug.

      **What this is not.** Not the receive mode: identical under both. Not the
      Wi-Fi: a flaky link shows as TCP resets or stub `recv` errors, not as an
      orderly halt-clear storm on one endpoint. This is protocol-level. The
      likeliest mechanism is the GIP handshake — the host must send a power-on
      packet over interrupt-OUT before the pad streams on interrupt-IN, and an
      OUT that does not land looks exactly like this: IN never produces, the
      driver resets the pipe, retries, gives up.

      **Scope.** One device, one usbip-win2 release. But the device is the *easy*
      case for the Phase 8 question: the Xbox Wireless Adapter also speaks GIP
      and additionally needs firmware upload and MT7612U radio bring-up through
      the same transport — strictly more URB traffic, not less. A transport that
      cannot carry a wired GIP pad is not going to carry the adapter.

      Not necessarily permanent. `usbip-win2` is actively developed and 0.9.7.8's
      notes record a device-specific URB fix ("Handle
      `_URB_CONTROL_VENDOR_OR_CLASS_REQUEST` to fix some devices"), so this is
      the kind of thing that gets fixed — worth filing upstream with this log.
      But it is not a foundation to build Phase 8 on today.
- [ ] **Control, optional:** forward the webcam (`04f2:b45d`, busid `1-8`)
      instead. If it attaches and stays, the transport is fine and the failure is
      GIP-specific, which sharpens the upstream report. Not needed for the
      decision.
- [~] **The Android userspace server is moot** until the Windows side carries
      GIP at all. Nothing to scope.

### Investigation B — HIDMaestro from Rust: **answered, and it is no**

Everything mechanical works. What does not exist is a contract.

**What was established, in order:**

- [x] **Installs and runs.** PadForge (same author) creates the pads; it always
      runs elevated and installs HIDMaestro in that session. No test-signing, no
      EV cert.
- [x] **The sections are created lazily.** A pad that has never been fed has *no*
      section — only `HIDMaestroCompanionInputEvent<N>`, which the XUSB
      companion's driver open-or-creates when its devnode appears.
      `EnsureInputMapping` builds the section and both events together, on first
      submit. Bind a controller and move a stick and all five objects appear.
- [x] **Rust can open and write them.** From an elevated process, read-back was
      400/400: the bytes are still ours immediately after writing.
- [x] **The layout is as transcribed.** Input is `SeqNo`/`DataSize`/`Data[256]`/
      `GipData[14]`/extended = 362 bytes. The output ring is 64 slots of 264, and
      **`SeqNo` is 1-based while `Head` is a count** — report *N* sits at position
      *(N−1) mod 64*, which one run showed directly as "position 0 holds SeqNo 1".
- [!] **Nothing reached `joy.cpl`.** And the reason is not the mechanism.

**Why it cannot be driven from Rust.** From the SDK reference:

> *"SubmitState translates the abstract HMGamepadState into the active profile's
> HID report layout and writes it to shared memory."*

So the bytes in that section are a **fully-formed, profile-specific HID report**,
not an abstract gamepad struct. Producing them is `HidReportBuilder.cs` — 53KB —
across **231 profiles**. `SubmitRawReport` exists for raw bytes, but it does not
remove the need to know the exact layout for the profile in play.

And the interface itself is not promised to anyone: **the SDK reference documents
no IPC, named pipe, C ABI or other non-.NET consumer surface**, and the shared
memory appears only as internal SDK mechanics rather than a public contract. It
has already moved once — the driver header notes the design "is now obsolete (the
driver and SDK communicate via shared memory, not IOCTLs)" — in five months.

**So using HIDMaestro means using its C# SDK**, which puts .NET, a second
process, and our own IPC on the *input path* rather than just at startup. That is
a materially heavier architecture than "Rust writes 278 bytes", and the lighter
version is only available by binding to an undocumented internal interface whose
report layout we would also have to reimplement.

> Two of my own errors are worth recording, because both were caught by the
> output contradicting the verdict rather than by review. A `SeqNo` delta of 336
> against 800 expected was printed directly beneath "the answer is yes" — the
> seqlock writer computed both values independently from the value it read, so an
> odd reading made it move the counter *backwards*. And the driver's doorbell,
> `HIDMaestroInputEvent<N>`, was never signalled at all, leaving the driver on its
> 50ms safety timeout while a 60Hz co-writer overwrote the section in between.
> Neither changes the conclusion; both would have made a positive result
> unreliable.

**Verdict for Phase 2: ViGEmBus.** ~6 ioctls against an ABI frozen by archival,
all Rust, no sidecar, no .NET.

**But "not drivable from Rust" was stated too absolutely.** The precise finding
is *not drivable via its internals*; its supported surface is the .NET SDK, and
that surface *can* be called from Rust by hosting .NET in-process — `netcorehost`
(maintained, 0.22.0) to embed the runtime, or a NativeAOT-compiled shim with
`[UnmanagedCallersOnly]` exports that Rust `LoadLibrary`s like any other DLL.
Either route uses `SubmitState`, so the 53KB report builder and the 600KB
orchestrator become the SDK's problem rather than ours, on the documented API
instead of an internal layout that has already moved once.

It is not free, and one cost is the reason it is a Phase 8 option rather than a
Phase 2 one:

- **Allocation on the input path.** The SDK reference describes `HMGamepadState`
  as holding an *"axes dict"*. A per-frame dictionary is a managed allocation
  per frame, which is GC, which is pauses on a path CLAUDE.md lists as hot under
  a zero-allocation rule. NativeAOT removes the JIT, not the GC. **Measure this
  before writing the shim.** ViGEm's 264-byte ioctl has no equivalent.
- A C# shim (~150 lines plus a reverse callback for the managed `OutputDecoded`
  rumble event): a new language in the repo.
- NativeAOT does not cross-compile Linux→Windows, so the DLL needs a Windows
  build step beside `cargo xwin`. `netcorehost` avoids AOT but needs .NET 10 at
  runtime — which HIDMaestro already requires, so on any machine that has it the
  dependency is already paid.

**Where this lands:** with USB/IP dead on GIP and the DS4 target making games see
a DualShock, a hosted HIDMaestro SDK is the one remaining route past the X360
ceiling that keeps the pad presenting as what it is. Recorded against Phase 8 as
the option, gated on the allocation measurement, not built.

### Where that leaves it — the spike is closed

- **Investigation B is closed for Phase 2**: HIDMaestro's internals are not a
  contract, and its supported surface is .NET. Hosting that SDK in-process
  (NativeAOT shim or `netcorehost`) is viable and is the Phase 8 option — gated
  on measuring per-frame managed allocation first.
- **Investigation A is closed**: USB/IP cannot carry the Xbox GIP protocol through
  `usbip-win2` 0.9.8.0 in either receive mode. The adapter is a strictly harder
  case than the wired pad that failed.
- **Pads go through ViGEmBus**, which was the fallback all along: ~6 ioctls
  against an ABI frozen by archival, all Rust, no sidecar.
- **Phase 8 stands as written** — vendored MT7612U radio, GIP in Rust, ViGEm X360
  on the server — and its X360 ceiling with it. The one route past the ceiling
  still unspent is ViGEm's DS4 target. USB/IP is recorded against Phase 8 as
  "revisit if upstream fixes GIP", not as a plan.

The direct pad baseline — p50 8.00ms, p99 12.00ms, 100% continuity — stays on
record. It is what the Phase 8 acceptance criterion ("input latency measured
against a directly-connected pad") will be compared to when the adapter path
exists.

---

## 9. Phase 3/4 — capture → encode → transport (env S, receiver on the Linux box)

The whole transport is host-tested and the Windows half compiles under
`cargo xwin`, but nothing here is measured. These run the server on the 4070 and
`fakeclient stream` on the Linux box across the wired LAN. Pair once, then:

```
fakeclient stream --server <box>:47811 --codecs hevc --out a.265 --secs 60 --stats
```

- [ ] **A session starts on `Hello`.** `SessionConfig` then `CodecPrivate`
      arrive; the session-key switch holds (input still verifies afterwards); the
      web UI Sessions panel shows it, and a UI disconnect stops it.
- [ ] **Sustained 4K60 HEVC, 30 min:** 0 abandons, 0 keyframes after the first,
      the instrumentation ring reports 0 dropped samples. Repeat `--codecs av1
      --out a.ivf`.
- [ ] **2% loss recovers by retransmission:** `--drop 2` -> 0 keyframes after the
      first, abandons ~ 0. `--drop 2 --no-retransmit` -> abandons occur and the
      server log shows invalidations, **not** forced IDRs, on HEVC at `dpb_depth
      8`.
- [ ] **Rate control converges and holds.** On the receiver:
      `tc qdisc add dev <nic> root tbf rate 80mbit burst 32kbit latency 50ms`.
      The bitrate `GET /api/sessions` reports falls under 80 Mbps within 2 s and
      does not oscillate; removing the qdisc lets it climb back.
- [ ] **USO on vs off:** the `send`-stage p99 with USO against
      `SUNBURST_NO_USO=1`, and which path actually ran.
- [ ] **PR-gate p99s** for the send, encode, capture and governor changes,
      against the pre-merge baseline (Section 4). Noise floor ~3%: take more runs
      or say "inconclusive" rather than quoting a 1% move.
- [ ] **AccessLost and secure desktop.** Alt-tab into a fullscreen game and back:
      the pipeline rebuilds and re-sends `CodecPrivate`, the stream continues.
      Lock the screen: `SecureDesktop{active:1}` then `{0}` on unlock, and the
      client never shows a frozen frame.
- [ ] **NvFBC spine** (after the real PTX is vendored, `SUNBURST_NVFBC=1`): the
      CUDA-native path streams. With the placeholder PTX it streams black -- the
      documented behaviour.
- [ ] **WGC frame-arrival wait:** confirm the event-signalled wait holds up under
      a real 4K60 run -- no missed frames, no added latency versus the DDA path.
- [ ] **Phase 3 playback:** mux the dumps (`ffmpeg -i a.265 -c copy a.mkv`; the
      `.ivf` plays directly) and confirm they play on the Shield and the Homatics
      with HDR active. This closes Phase 3's "both streams play on their target
      device" criterion.
- [ ] The instrumentation Section 4 rows for env S can be filled from these runs.

Not covered here, and why: **client-side cursor rendering** landed in Phase 5
(server-side GDI capture and the client overlay both), validated in §10 rather
than here. **HDR mastering in `SessionConfig`** is `None` for now; the encoder
writes the ST 2086 metadata into the bitstream, and populating the handshake
block from the display is a Phase 5 refinement.

---

## 10. Phase 5 — the Android client, glass to glass (env A and B)

The client is built, packaged and clippy-linted for both ABIs, but nothing here
is measured. Sideload the debug APK on the Shield (HEVC) and the Homatics (AV1),
server on the 4070, wired LAN.

- [ ] **Pairs from the TV.** Arm pairing in the web UI; the app shows a PIN; type
      it into the UI; the client stores the secret and reconnects paired after a
      restart. A wrong PIN fails to stream (the derived secrets differ).
- [ ] **Streams and decodes.** 4K60 to the `SurfaceView`: HEVC on the Shield, AV1
      on the Homatics. Watch for the av1C silent-failure (configures, outputs
      nothing) — a black screen with no decoder error is that.
- [ ] **HDR end to end.** `Display.HdrCapabilities` positive, the panel enters
      HDR, colours and highlights correct, no UI-text chroma fringing. (The
      mastering rides in the bitstream today; `SessionConfig.hdr` is not yet
      populated server-side — that is the KEY_HDR_STATIC_INFO path, ready and
      inert.)
- [ ] **No microstutter over 30 minutes** — the vsync-timed `releaseOutputBuffer`,
      not immediate release. If it stutters, revisit the present timestamp and
      the jitter-buffer min depth.
- [ ] **Input in a real game.** Keyboard, mouse (relative via captured pointer,
      wheel, buttons), and a gamepad all work in Steam Big Picture. Enhanced
      Pointer Precision off on the server (CLAUDE.md).
- [ ] **Glass-to-glass measured** (high-speed camera or LED-on-input rig), within
      the 60–100 ms budget. The §4 client instrumentation rows
      (recv/jitter/dec-submit/dec-out/present) fill from the same runs.
- [ ] **Cursor renders client-side** from the server's CursorShape/CursorPosition
      and tracks; a changed shape (pointer/hand/text) updates. Monochrome cursors
      and no-alpha shapes are the ones to eye. Local prediction (moving the
      overlay from the client's own mouse deltas) is the refinement if the
      round-tripped position feels laggy.
- [ ] **Decoder quirks.** These are not probeable (decoder behaviour, not
      capability): confirm the Shield takes reference invalidation and the
      Homatics AV1 takes intra-refresh; on the Homatics HEVC, retest with periodic
      IDR before concluding the decoder is broken (CLAUDE.md). Set the enabling
      quirks per device once confirmed.
- [ ] **PerformanceHintManager** keeps the client thread on a fast core on the
      Amlogic — check for a frame-time improvement with it on vs. off.

---

## 11. Phase 6 — audio, A/V sync (env S, A and B)

Audio is built end to end but nothing here is measured. Server on the 4070 at a
48 kHz output; the debug APK on both TVs; a game with clear, positional audio
(and something with visible lip movement for the sync check).

- [ ] **Sound reaches both TVs**, decoded from Opus and played through AAudio
      `LowLatency`, on the Shield (HEVC video) and the Homatics (AV1 video).
- [ ] **No A/V drift over 30 minutes.** Lip-sync holds start to finish; the
      bounded-buffer corrections (drop-newest past the watermark, silence on
      underrun) are inaudible. If a constant lip-sync *offset* is visible, tune
      the ring watermark/capacity in `audio.rs` — that offset is the box-tuned
      number the plan left open, not drift.
- [ ] **Idle gap.** Go silent then loud (a menu, then gameplay): audio stops and
      resumes cleanly, no desync on resume (the server's injected silence held
      the cadence).
- [ ] **Host-mute via device selection.** With `StreamConfig.audio_device` unset,
      the server's own speakers play the game (default endpoint) and the client
      still gets audio. Set it to "Steam Streaming Speakers" (with Steam's
      Remote Play components installed): the server's physical output goes silent
      and the client still gets audio. Confirm the device is present/enabled
      first; a missing device logs a warning and falls back to the default.
- [ ] **48 kHz requirement.** Set the server output to 44.1 kHz and confirm the
      startup warning fires (resampling is deliberately not implemented); restore
      48 kHz.
- [ ] **Loss recovery.** Induce ~2% loss: audio recovers via Opus FEC/PLC with no
      audible dropout, and **no** audio NACKs are sent (audio has none).
- [ ] **Audio latency measured** from the new instrumentation chains
      (au-capture/au-encode/au-send on the server, au-recv/au-decode/au-play on
      the client), reported alongside the §4 video rows from the same runs.

---

## 12. Phase 7 — display integration + optional virtual display (env S)

The display and VDD code is `cargo xwin`-verified but its behaviour is not.
Server on the 4070; run these against a real HDR display and, for the VDD rows,
with the MikeTheTech Virtual Display Driver installed.

- [ ] **HDR around the session.** A client connects for an HDR stream: the
      desktop enters HDR; on disconnect it returns to exactly the prior state
      (on and off, both directions). Kill the server mid-session — HDR still
      restores (the guard's `Drop`).
- [ ] **Resolution matching** (opt-in `match_resolution`). The desktop switches
      to the client's resolution/refresh for the session and restores after. An
      unsupported mode is refused (CDS_TEST) rather than blanking the screen.
- [ ] **Per-app profile.** Launch a game whose entry has a bitrate/codec
      override; the live stream uses it, not the global default (codec only
      applies when the game is launched before the client connects). The game
      exits and its prep is undone (the `try_wait` reap).
- [ ] **Virtual display** (opt-in `virtual_display`). With the VDD installed, the
      session enables it and — with `capture_output` set to its DXGI index —
      streams a client-native-resolution virtual display; the physical display is
      untouched; the VDD is disabled on disconnect. Headless (no physical monitor)
      still streams. Confirm the VDD's hardware id matches `VDD_HARDWARE_IDS` in
      `display.rs`; add it there if a newer build differs.
- [ ] **Fallback.** With `virtual_display` on but no VDD installed, the server
      logs a warning and streams the physical display.

---

## 13. H.264 — a low-latency SDR codec (env S, A and B)

H.264 is built end to end but its NVENC config, the NV12 convert (HLSL + the
placeholder CUDA kernel) and the tonemap are `cargo xwin`-verified only. Set the
server codec preference (or a per-app override) to H.264.

- [ ] **Negotiate + decode.** A client offering `video/avc` negotiates H.264 and
      decodes it; HEVC/AV1 still negotiate when preferred; auto never picks H.264
      over an HDR codec.
- [ ] **Correct 8-bit SDR** (BT.709 colours, no cast) on the DDA/WGC path. On the
      NvFBC path once the real `argb_to_nv12` PTX is vendored (the checked-in one
      is a no-op placeholder — a black frame, by design, like the P010 kernel).
- [ ] **Tonemapping.** With the desktop in HDR, an H.264 stream still looks right
      (highlights rolled off, no clip/oversaturation); in SDR it is unchanged. The
      session does not alter the desktop's HDR state.
- [ ] **Latency vs HEVC** on the same content, from the §4 instrumentation rows —
      the reason H.264 exists. Report the delta.
- [ ] 2% induced loss recovers (NACK + H.264 reference invalidation, which shares
      HEVC's `Window`).
- [ ] A per-app profile pins H.264 for one game while others stay HDR (HEVC/AV1).

## 14. Configuration surface — everything takes effect (env S, A and B)

The config is honoured live per session (no restart), and the round-trip, the
device picker and the override merge are already host-tested through the `Fake`.
What only real hardware can show is that each knob *moves the right number* and
that the defaults reproduce today's behaviour. Read the effect off the §4
instrumentation rows where latency is involved.

- [ ] **NVENC preset.** P1 → P4 raises encode time and, at a fixed bitrate,
      quality; P1 is unchanged from the old hardcoded value. All four stay within
      the ULL envelope (no UHQ/B-frame regression in the numbers).
- [ ] **Rate control.** VBR spends less on static frames than CBR; neither adds a
      latency spike. CBR remains the default.
- [ ] **Capture backend.** Forcing WGC, DDA and NvFBC each streams; `Auto`
      reproduces the OS default (WGC on Win11, DDA on Win10). Backends measure
      within the §7 half-millisecond of each other; NvFBC is the 1.2–1.5 ms-worse
      resilience path, as documented — confirm it is not silently the default.
- [ ] **Slices / IDR / DPB / fps cap.** A non-zero slice count changes the slice
      layout on the wire; a forced IDR period shortens the GOP; an fps cap holds
      the encode rate below the client's refresh. Defaults (0/0/8/0) are today's.
- [ ] **Audio codec knobs.** Opus frame duration (2.5/5/10/20 ms), FEC and
      complexity change the audio packet cadence/robustness; 5 ms/FEC-on/10 stays
      the default, and A/V sync (§11) holds across the range.
- [ ] **Audio device picker.** `GET /api/audio-devices` lists the server's
      endpoints; selecting one captures it; "Steam Streaming Speakers" silences
      the host while the client keeps audio; the default endpoint leaves the host
      audible. A name no longer present falls back to the default, not silence.
- [ ] **Input tuning.** Mouse sensitivity scales injected relative deltas (1.0 is
      1:1); gamepad deadzone widens the neutral zone; the EPP toggle turns
      Enhanced Pointer Precision off for the session (verify in the OS mouse
      settings) and restores it on disconnect.
- [ ] **Client requests, clamped.** From the TV settings screen a codec request
      is honoured only if the device can decode it (else negotiation falls back);
      a bitrate ceiling only lowers the rate, never raises it past the server cap.
      Jitter depth, cursor overlay and the performance hint apply on the client;
      changing codec/bitrate/jitter reconnects, the presentation prefs do not.
- [ ] **Per-app overrides.** A per-app codec/bitrate/preset applies only while
      that app is the running one; other apps inherit the global defaults.
- [ ] **Autostart.** Toggling it from the UI creates/removes the scheduled task.

---

## 15. Phase 8 — Xbox pads (env S + the adapter, wired pads, a headset; both TVs)

`sunburst-gip-bridge` builds for both ABIs and links the vendored C++ on the dev
box, but the radio, real pads and the headset are hardware-only. Runs on the
Android client (env A/B) against the server (env S). One clip is worth the whole
list: a wired pad, then the adapter with a headset, through Big Picture.

- [ ] **Wired pads enumerate and play.** A wired Xbox One and an Xbox Series pad
      each attach (`UsbManager` permission prompt → fd), drive a game through Big
      Picture, and disconnect cleanly. The server creates the emulated pad exactly
      as for a TV-native controller.
- [ ] **The adapter brings up the radio and pairs.** It loads the fetched
      `FW_ACC_00U.bin`, brings up the MT7612U radio, and pairs a pad from the TV
      remote's "Pair a controller" action (the physical button on the unit here is
      dead). Up to **four pads** play at once.
- [ ] **Rumble, including the trigger motors.** Rumble arrives and stops cleanly;
      a lost final zero-level packet self-heals via the 200 ms client timeout /
      100 ms server repeat. The Xbox **impulse-trigger** motors fire (a game that
      uses them, e.g. a racing/shooter trigger effect).
- [ ] **Battery.** Level, charging, and headset-present flags show per pad and
      track a pad going flat / onto the charger.
- [ ] **Headset, both directions (≤2 pads).** Server audio is audible in the pad's
      headphones at the negotiated format, with working volume and the TV/pad/both
      routing; the pad **mic** reaches the server's chosen "Steam Streaming
      Microphone" endpoint and is heard by an app reading it. Confirm the mic is
      the native 24 kHz mono capture upsampled cleanly by the server's decoder (no
      pitch/speed artefact). No A/V drift over 30 min (§11 sync); survives an audio
      reconfiguration (unplug/replug the headset mid-session).
- [ ] **Input survives a UAC prompt and a lock/unlock.** The pad keeps driving the
      re-attached desktop (the §6 desktop-reattach path), and Steam Input is
      characterised (does it double-enumerate the emulated pad in any game?).
- [ ] **Latency vs a directly-connected pad** is measured and the difference
      reported (button-to-action, against a pad plugged into the server).
- [ ] **Record a GIP frame corpus.** With the driver working, capture real GIP
      frames (input, rumble, the audio handshake, capture/render audio) into a
      fixture. This seeds the deferred **GIP-in-Rust** rewrite: the Rust layer is
      validated by reproducing this verified corpus and diffed against the C++
      on-device before it swaps in behind the bridge seam.

---

## Hardware still needed

| Needed for | Hardware |
|---|---|
| §1's NvFBC row | The 4070 box; `--enable-nvfbc` needs elevation |
| §7 latency | The 4070 box, screen left alone while it runs |
| §8 HIDMaestro | HIDMaestro installed and its cert trusted; a pad created |
| §8 USB/IP | `usbip-win2` ≥ 0.9.8.0 on the box, `usbip` here, a USB device to forward |
| §2 | Both Android boxes, adb reachable |
| §4 client rows | Phase 5 client, so not yet |
| Phase 8 | Xbox Wireless Adapter (`045e:02e6`) and up to four pads |
| Glass-to-glass (Phase 5) | High-speed camera, or an LED-on-input rig |

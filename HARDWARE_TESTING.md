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

Run `spikes/probe-windows`. It answers more than the roadmap asked for, because
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
spikes/check-phy.sh [adb-serial]
```

- [x] **Shield: link is gigabit.** 216 Mbps measured, well past a 100Mbit PHY's
      ~94 Mbps ceiling. *(Verified over adb-over-TCP.)*
- [!] **Homatics: still unanswered — the box is on Wi-Fi.** `eth0` exists and the
      Ethernet service is enabled, but the active network is `wlan0`: SSID
      "House LANister", 5240 MHz, Wi-Fi 6, RSSI −61, 1200 Mbps PHY rate. **The
      gigabit port is present and unused.**

      The script measured 494 / 119 / 136 Mbps across three attempts and its own
      warning fired first — *"no ethernet interface… the number below is a wi-fi
      measurement and does not answer Phase 0.3"*. That warning earned its place:
      494 Mbps is comfortably past a 100Mbit ceiling and would have read as a
      clean pass. The 4× spread across attempts is the giveaway a wired link
      would not produce.

      **Plug it in and re-run.** A gigabit port on the spec sheet is not the
      question 0.3 asks — the question is what the link actually negotiates, and
      the failure mode it exists to catch is a bad pair negotiating 100 on
      gigabit-capable hardware at both ends.
- [ ] Confirm the switch port agrees, not just the box — a single bad pair
      negotiates 100 and looks exactly like a hardware limit.
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
| Homatics | 494 Mbps peak, over **Wi-Fi** | **does not answer 0.3** — wire it and re-run |

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

## Hardware still needed

| Needed for | Hardware |
|---|---|
| §1's NvFBC row | The 4070 box; `--enable-nvfbc` needs elevation |
| §3 Homatics | **An Ethernet cable.** The port is there and unused; the box is on Wi-Fi |
| §2 | Both Android boxes, adb reachable |
| §4 client rows | Phase 5 client, so not yet |
| Phase 8 | Xbox Wireless Adapter (`045e:02e6`) and up to four pads |
| Glass-to-glass (Phase 5) | High-speed camera, or an LED-on-input rig |

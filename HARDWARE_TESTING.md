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
| S | Windows server | | RTX 5070 non-Ti (GB205). Record the driver version — the AV1 GUIDs and Blackwell caps need r570+. |
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

Run `spikes/probe-windows`. It answers more than the roadmap asked for, because
the extra questions cost nothing once the encoder session is open and each one
de-risks Phase 3.

- [ ] **Driver is r570 or newer.** Older headers lack the AV1 GUIDs entirely, so
      every AV1 answer below would be a false negative.
- [ ] **NvFBC availability.** Assume unavailable until proven otherwise —
      NVIDIA deprecated it on Windows and points at Desktop Duplication.
      Available makes it priority-1 and worth more here than on a Ti, since
      encode latency is a fixed floor and DWM composition becomes the biggest
      remaining target. Unavailable deletes the backend from the plan.
- [ ] **HEVC caps**: `SUPPORT_REF_PIC_INVALIDATION`, `SUPPORT_INTRA_REFRESH`,
      subframe/slice output, 10-bit, max bitrate.
- [ ] **AV1 caps**: the same list. **Do not assume parity with HEVC.** This is
      the one that matters most — the Homatics has no HEVC path behind it, so an
      AV1 encoder without reference invalidation means NACK recovery there
      degrades to `RequestIdr`, and that changes Phase 4.
- [ ] **Second NVENC session detected** when ShadowPlay, Instant Replay or OBS is
      running. The driver time-shares one physical encoder and per-frame times get
      jittery in a way that reads exactly like our bug, so this becomes a startup
      warning.

| Cap | HEVC | AV1 |
|---|---|---|
| Ref pic invalidation | | |
| Intra refresh | | |
| Subframe readback | | |
| 10-bit | | |
| Max bitrate | | |

---

## 2. Phase 0.2 — Decoder enumeration (env A and B)

Run the enumeration activity in `android/` on both boxes and pull the output
file. This seeds `DecoderQuirks` with measurements instead of assumptions.

- [ ] **Every decoder enumerated, not just the first match.** Errata #15 in
      `decoder-errata.txt`: some devices do not support `FEATURE_LowLatency` on
      their first compatible decoder, and picking the first one silently loses it.
- [ ] For each HEVC/AV1 decoder record: name, `isHardwareAccelerated`, profiles,
      levels, max resolution, `FEATURE_LowLatency`, and whether `KEY_LOW_LATENCY`
      is actually **accepted** rather than merely advertised.
- [ ] **Shield: HEVC Main10 at 4K60 is present.** It has no AV1 block, so this is
      its only path.
- [ ] **Homatics: AV1 Main10 at 4K60 is present.** Same — its only path.
- [ ] **Vendor low-latency keys** checked on the Amlogic. Errata #16/#17: some
      Amlogic decoders produce no output at all without an undocumented
      `MediaFormat` option, which reads as a broken stream rather than a missing
      flag.

| Device | Decoder | HW | LowLatency | KEY_LOW_LATENCY accepted | Max res |
|---|---|---|---|---|---|
| | | | | | |

---

## 3. Phase 0.3 — Homatics network PHY (env B)

Run `spikes/check-phy.sh`.

- [ ] **Link negotiates 1000 Mbps, not 100.** A 100Mbit PHY caps usable
      throughput around 80 Mbps, which would sit below the AV1 target range and
      reshape the bitrate plan rather than merely tightening it.
- [ ] Confirm the switch port agrees, not just the box — a bad cable shows up
      here and nowhere else.

Result: _______ Mbps

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

## Hardware still needed

| Needed for | Hardware |
|---|---|
| §1 in full | The 5070 box with r570+ |
| §2 | Both Android boxes, adb reachable |
| §4 client rows | Phase 5 client, so not yet |
| Phase 8 | Xbox Wireless Adapter (`045e:02e6`) and up to four pads |
| Glass-to-glass (Phase 5) | High-speed camera, or an LED-on-input rig |

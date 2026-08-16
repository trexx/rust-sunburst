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
- [ ] **Homatics: AV1 Main10 at 4K60 is present.** Its only path, since the HEVC
      decoder is broken.
- [ ] **Vendor low-latency keys** on the Amlogic. Errata #16/#17: some Amlogic
      decoders produce no output at all without an undocumented `MediaFormat`
      option, which reads as a broken stream rather than a missing flag.

### Shield results (verified)

| Decoder | MIME | HW | FEATURE_LowLatency | KEY_LOW_LATENCY | 4K60 | Max res |
|---|---|---|---|---|---|---|
| `OMX.Nvidia.h265.decode` | hevc | yes | **yes** | silent | **yes** | 3840×2176 |
| `OMX.Nvidia.h265.decode.secure` | hevc | yes | no | silent | yes | 3840×2176 |
| `OMX.google.hevc.decoder` | hevc | no | no | rejected | no | 4096×4096 |

`OMX.Nvidia.h265.decode` is the target. Profile list includes `4096`
(`HEVCProfileMain10`). The software decoder cannot do 4K60 and is not a fallback.

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
- [ ] **Homatics: link is gigabit, not 100 Mbit.** This is the one Phase 0.3
      actually asks about. A 100Mbit PHY caps usable throughput around 80 Mbps,
      below the 70–100 Mbps AV1 target rather than merely tightening it.
- [ ] Confirm the switch port agrees, not just the box — a single bad pair
      negotiates 100 and looks exactly like a hardware limit.

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
| Shield | 216 Mbps | gigabit |
| Homatics | | |

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

## Hardware still needed

| Needed for | Hardware |
|---|---|
| §1 in full | The 5070 box with r570+ |
| §2 | Both Android boxes, adb reachable |
| §4 client rows | Phase 5 client, so not yet |
| Phase 8 | Xbox Wireless Adapter (`045e:02e6`) and up to four pads |
| Glass-to-glass (Phase 5) | High-speed camera, or an LED-on-input rig |

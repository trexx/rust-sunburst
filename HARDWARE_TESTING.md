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
| S | Windows server — the target | | RTX 5070 non-Ti (GB205). Record the driver version — the AV1 GUIDs and Blackwell caps need r570+. |
| S4 | Windows box §1 was measured on | NVENC API 13.1 | **RTX 4070, 12 GiB — Ada, not the target.** Same shape as S for what §1 asks: one NVENC, AV1 encode present. Its answers carry; Blackwell-specific caps are still owed a re-run on S. |
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

## 1. Phase 0.1 — Encoder and capture capabilities (env S4; re-run owed on S)

```
probe-windows                  # everything, no side effects
probe-windows --enable-nvfbc   # adds NvFBC_Enable: needs elevation, resets the driver
```

Run `spikes/probe-windows`. It answers more than the roadmap asked for, because
the extra questions cost nothing once the encoder session is open and each one
de-risks Phase 3.

**Measured on env S4 — an RTX 4070, not the 5070.** Recorded rather than
discarded, because the answers below are ones the two parts share by
construction: both are single-NVENC consumer cards with an AV1 encoder, and
Blackwell's encoder feature set is a superset of Ada's. That is enough to close
every *design* question Phase 0.1 was there to gate. It is not enough to satisfy
CLAUDE.md's own rule — **query, do not assume parity** — so the matrix gets
re-run on S when that box is available, and a difference is then a finding
rather than a surprise.

- [x] **Driver is new enough.** The probe reports **NVENC API 13.1**, above the
      13.0 this build targets — which is the check the startup version gate
      actually performs, and the one that gates the AV1 GUIDs. The probe prints
      the API version, not the driver version string; if the r570 number itself
      is ever wanted, that is a line to add to the probe rather than a fact to
      infer from this.
- [!] **NvFBC: the first answer was wrong, and the question is reopened.** The
      probe reported "no `NvFBCCreateInstance`, treat as unavailable". That was
      a **false negative from asking the wrong entry point**: there are two
      generations of NvFBC, `NvFBCCreateInstance` belongs to the 7.x/Linux one,
      and the Windows API the driver still exports is the legacy
      `NvFBC_CreateEx` / `NvFBC_GetStatusEx` / `NvFBC_Enable` set. A missing
      modern export is a *version detection*, not a verdict.

      The probe now walks the legacy path, and legacy NvFBC is gated to
      professional cards by a private-data key. It asks **both ways** and prints
      the pair, because a keyed success only means something next to an unkeyed
      failure — a keyed-only run proves nothing, since a Quadro passes either
      way. Re-run and record the four lines it prints (`GetStatusEx` and
      `CreateEx`, unkeyed and keyed).
- [ ] **`--enable-nvfbc`, only if `CreateEx` refuses while status says capture is
      possible.** `NvFBC_Enable` needs elevation and **resets the display driver**,
      which on a box someone is watching is indistinguishable from a crash — so
      the probe never calls it unless asked by name.
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

**One encoder engine, as expected on a non-Ti.** No Split Frame Encoding. Encode
time stays a fixed 5–10ms floor, which is what makes the line above load-bearing
rather than an optimisation.

**NvFBC is undecided, and that is the one open item from 0.1.** DWM composition
is ~16.7ms in CLAUDE.md's budget — the largest single line in the table, larger
than encode — and NvFBC and swapchain hooking are the only two ways past it. So
which way this lands matters more here than it would on a Ti, and it is worth
the re-run rather than the assumption.

**What a keyed success would and would not license.** It would not make NvFBC a
priority-1 backend, for three reasons that are all independent of whether the
call succeeds:

- The key is undocumented and can stop working on any driver update. A capture
  backend that can vanish in a driver release is not a default; at most it is an
  opt-in fast path behind one that always works.
- NVIDIA deprecated NvFBC on Windows from the Windows 10 October 2019 update
  onward, and CLAUDE.md's floor is Windows 10 1903+ — so essentially the whole
  supported OS range is past the deprecation.
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
| §1 in full | The 5070 box with r570+ |
| §2 | Both Android boxes, adb reachable |
| §4 client rows | Phase 5 client, so not yet |
| Phase 8 | Xbox Wireless Adapter (`045e:02e6`) and up to four pads |
| Glass-to-glass (Phase 5) | High-speed camera, or an LED-on-input rig |

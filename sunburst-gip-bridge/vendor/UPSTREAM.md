<!-- SPDX-License-Identifier: GPL-2.0-or-later -->
# Vendored driver provenance

`vendor/xow/` is the userspace Xbox One wireless dongle driver from
[medusalix/xow](https://github.com/medusalix/xow), **GPL-2.0-or-later**, by way of
its Android port in [moonlight-trexx](https://github.com/) (the `xow_driver`
subtree). Sunburst is GPL-2.0-or-later, so this is licence-compatible.

    Upstream:      https://github.com/medusalix/xow
    Baseline:      commit d335d6024f8380f52767a7de67727d9b2f867871 (2022-04-24)
    Via:           moonlight-trexx app/src/main/jni/xow_driver (Android-adapted)
    License:       GPL-2.0-or-later

`vendor/libusb/` is libusb (LGPL-2.1-or-later), the source subtree plus the
Android `config.h`, built by `build.rs`. See `vendor/libusb/COPYING`.

**Do not overwrite these files wholesale from upstream** — the Sunburst port is
not a clean copy. Differences from the moonlight-trexx port:

## Not vendored

- `xow_driver_jni.cpp` — the JNI entry layer. Replaced by `vendor/shim/sb_gip_shim.cpp`,
  a plain-C seam Rust binds (`src/ffi.rs`).
- `utils/jni.h` — JNI helper. Gone; the driver makes no Java calls.
- `dongle/firmware.cpp` — the embedded `FW_ACC_00U` blob. **Firmware is not
  committed**; `scripts/fetch-firmware.sh` stages it and `mt76.cpp::loadFirmware`
  reads it from a path (see below).
- `utils/crypto.cpp` — the Java/mbedtls handshake crypto. Replaced by
  `vendor/shim/crypto.cpp`, which forwards to the Rust `sb_crypto_*` FFI
  (`src/crypto.rs`, RustCrypto, host-tested byte-exact to `GipCrypto.java`).

## Local modifications (Sunburst)

- **De-JNI'd** `controller.{cpp,h}`, `dongle.{cpp,h}`, `wired.{cpp,h}`, `usb.h`:
  the per-pad Java object + `updateInput`/`updateBattery`/`audioDeviceRemoved`
  `CallVoidMethod` upcalls and the `notifyJavaControllerAdd`/`Remove` calls are
  replaced by a C++ `GipSink` (`vendor/shim/sb_gip_shim.h`); constructors take a
  `GipSink*` instead of `jobject`/`JavaVM`; the read/metadata/volume threads no
  longer attach to a JVM (nothing on them calls into Java any more).
- **`mt76.cpp::loadFirmware`** reads the firmware from its `firmwarePath`
  argument (upstream ignored it and used the embedded blob).
- **`crypto.h`** drops `init(JNIEnv*, jclass)` and `<jni.h>`; the five primitives
  are implemented in `vendor/shim/crypto.cpp` over the Rust FFI.
- **Microphone capture retention** (`[MS-GIPUSB]` 3.2.5.1.4). Upstream read only
  the flow rate from Audio Capture messages and discarded the mic PCM ("this client
  has no microphone"). Sunburst retains it: `Controller::audioSamplesReceived` gains
  a `pcm`/`pcmLen` slice (widened through the `GipDevice` virtual + `gip.cpp`
  dispatch) for the wireless path, and `WiredController::captureTransferComplete`
  keeps the PCM after the flow rate for the wired path; both feed a bounded per-pad
  ring (`Controller::captureAudioReceived`) drained by `Controller::drainCapturedAudio`
  → `sb_gip_audio_in`. The samples stay in their **native capture format** (a chat
  headset's mic is 24 kHz mono, code `0x09`); `Controller::captureFormatCode` /
  `sb_gip_mic_format` expose it so the client encodes at that rate and the server's
  48 kHz Opus decoder resamples — no resampler in the driver.

## The four `[MS-GIPUSB]` defects — audited, resolved

The original plan carried a "fix xow's four spec defects in-place" step. Audited
against the spec PDF (v20240916) and the vendored code, all four are resolved:
this port descends from the already-polished moonlight-trexx tree, which fixed
them, so no separate fixing step was needed. Status:

- **Rumble as a raw byte, not a percentage — fixed.** `controller.cpp`'s
  `RUMBLE_SCALE` maps the 16-bit magnitude onto the protocol's 0–100 range (spec
  §3.1.5.6.1, "Percentage, 0 - 100% of PWM"), applied to all four motors incl. the
  impulse triggers in `sendRumble`.
- **Wired sign-extension discontinuity — not present.** The wired read thread feeds
  reports to the same `GipDevice::handlePacket` / `InputData` decode as the dongle
  (`wired/wired.cpp` `readPackets`), whose stick fields are `int16_t` (`gip.h`):
  signed uniformly, no hand-rolled sign extension anywhere.
- **Dropped extended status message — fixed.** `gip.cpp` accepts any `CMD_STATUS`
  payload `>= sizeof(StatusData)` rather than exactly `0x04` (spec §3.1.5.5.2.2:
  all new GIP devices MUST use the extended status form), so battery and fault
  events from newer pads are no longer dropped whole.
- **Unadvertised host capabilities — no conformance gap.** The spec has no host
  capability advertisement: §1.7 (Versioning and Capability Negotiation) is "None",
  and the Hello (§1.8) is the *device* advertising to the host. The only capability
  mechanism, "Get Capabilities" (§3.1.5.5.10.1), is an *optional* host→device query
  for which Extended Commands the device implements; the driver deliberately does
  not send it (nothing here needs the serial-number/telemetry commands it would
  reveal — see the note in `gip.cpp`'s extended-command sender). The device's own
  advertised capabilities are read from metadata and logged.

Any future in-place fix lands here with a note.

<!-- SPDX-License-Identifier: MIT -->

# Vendored HIDMaestro data

Sunburst's Phase 2 gamepad support is a Rust port of HIDMaestro's userspace SDK
(see `sunburst-input/src/pad/`). The port reuses HIDMaestro's **data** verbatim —
its controller profiles are the single source of truth for on-wire report
layouts, and the Rust codec ([`sunburst_input::pad::codec`]) walks them exactly
as HIDMaestro's `VendorBlobCodec` does.

This directory holds that vendored data, following the same provenance rule as
the Xbox dongle firmware (`scripts/fetch-firmware.sh`): third-party material is
recorded with its origin and pinned, never silently absorbed.

## Provenance

| | |
|---|---|
| Upstream | https://github.com/hifihedgehog/HIDMaestro |
| Version | v1.8.0 (`dcdf9b48dd6f9c94e15aaba02fb8568417ed4db9`) |
| License | MIT (single `LICENSE` at the repo root — no per-directory split) |

HIDMaestro is MIT throughout, which is GPL-2.0-or-later compatible, so vendoring
its profiles into this GPL-2.0-or-later project is clean. The MIT license text is
reproduced in `LICENSE.HIDMaestro`.

## What is here

- `profiles/**/*.json` — **all 234** HIDMaestro profiles, verbatim. Each declares a
  controller's identity, HID `descriptor`, and either an `extendedReport` (16
  profiles: Sony BT / Valve — walked by `sunburst_input::pad::codec`) or a
  `descriptor` + `layout`/`buttonMap`/`axisMap` (the other 214, incl. Xbox — packed
  by `sunburst_input::pad::report` via the parsed descriptor). The 16 `extendedReport`
  profiles are proven byte-for-byte by the codec's golden-hash test; the descriptor
  path is verified by assertion (transcribed HIDMaestro probes + hand-decoded
  descriptors — no SHA-golden table exists for it).
- `driver/hidmaestro.inf`, `driver/hidmaestro_xusb.inf` — the driver install
  manifests, verbatim.

### Pending: the driver binaries

The driver is a **UMDF2 user-mode driver — `HIDMaestro.dll`**, not a kernel `.sys`
(it loads as a lower filter under Windows' own `MsHidUmdf.sys` / `WUDFRd`), plus its
XUSB companion **`HMXInput.dll`**. HIDMaestro does not commit them — its
`scripts/build.cmd` compiles `driver/driver.c` → `HIDMaestro.dll` and
`scripts/build_companion.cmd` compiles `driver/companion.c` → `HMXInput.dll` with
`cl.exe` + the WDK (UMDF 2.15), and `DriverBuilder` then embeds them as resources in
`HIDMaestro.Core.dll`.

They can't be produced on this Linux host, but they need not be *built* at all:
HIDMaestro publishes prebuilt GitHub releases, and these DLLs ship embedded as
managed resources inside `HIDMaestro.Core.dll`. So `.github/workflows/driver.yml`
downloads a release, extracts `HIDMaestro.dll` + `HMXInput.dll` from the assembly's
manifest resources (a direct metadata read — it neither runs HIDMaestro's code nor
needs the WDK), and uploads them; they are then vendored here with a pinned version
+ SHA-256. (Fallback if a release ever stops embedding them: build from the source
above with the WDK via HIDMaestro's `scripts/build.cmd`.) The Rust port compiles and
is fully tested without them; they are only needed to exercise a live device on the
box (with the held-back `device.rs` / `install.rs`).

## Why the profiles are the source of truth

HIDMaestro's own byte-parity test (`test/probes/vendor_blob_golden_check`) SHA-256
hashes three encoded frames per profile and compares against a committed golden
table. The Rust port reproduces that proof: same profiles in, same deterministic
state, same hashes out. If a hash diverges, the port has drifted from HIDMaestro's
wire format — which a real game would reject — so the proof is the contract, and
it runs on Linux with no driver, no device, and no .NET.

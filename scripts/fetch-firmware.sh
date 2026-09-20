#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-or-later
#
# Fetch the MT7612U firmware for the Xbox Wireless Adapter and stage it as an
# Android asset. The firmware is Microsoft's, so — following xone's reasoning and
# CLAUDE.md "Licensing" — it is NOT committed to this repo: it is extracted at
# build time from Microsoft's own driver package. The output path is gitignored.
#
# It lives inside the Windows driver .cab as FW_ACC_00U.bin; we extract it and
# copy it to android/app/src/main/assets/xone_dongle_fw.bin, which the client
# copies to its files dir at runtime and hands to `Bridge::open_dongle`.
#
# Usage:  scripts/fetch-firmware.sh [dest-file]
# Needs:  curl (or wget), cabextract (or bsdtar), sha256sum.

set -euo pipefail

# Microsoft's "Xbox Wireless Adapter for Windows" driver package. This URL is the
# one xone has used for years; it is plain http, so integrity rests on the hash
# check below, not the transport.
DRIVER_URL="http://download.windowsupdate.com/c/msdownload/update/driver/drvs/2017/07/1cd6a87c-623f-4407-a52d-c31be49e925c_e19f60808bdcbfbd3c3df6be3e71ffc52e43261e.cab"

# SHA-256 of the extracted FW_ACC_00U.bin, pinned so a reissued or tampered
# package is rejected — the download is plain http, so integrity rests here.
# Cross-checked against xone's install/firmware.sh (medusalix/xone). Override with
# SUNBURST_FW_SHA256 if Microsoft ever reissues the firmware (update xone's
# reference too), or set it empty there to fetch unverified.
EXPECTED_SHA256="${SUNBURST_FW_SHA256:-48084d9fa53b9bb04358f3bb127b7495dc8f7bb0b3ca1437bd24ef2b6eabdf66}"

FIRMWARE_NAME="FW_ACC_00U.bin"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="${1:-$REPO_ROOT/android/app/src/main/assets/xone_dongle_fw.bin}"

die() {
    echo "fetch-firmware: $*" >&2
    exit 1
}

have() { command -v "$1" >/dev/null 2>&1; }

have sha256sum || die "sha256sum is required"
if ! have cabextract && ! have bsdtar; then
    die "need cabextract or bsdtar to unpack the driver .cab"
fi
if ! have curl && ! have wget; then
    die "need curl or wget to download the driver"
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

echo "fetch-firmware: downloading the driver package…"
cab="$work/driver.cab"
if have curl; then
    curl -fSL --retry 3 -o "$cab" "$DRIVER_URL"
else
    wget -O "$cab" "$DRIVER_URL"
fi

echo "fetch-firmware: extracting $FIRMWARE_NAME…"
if have cabextract; then
    cabextract -d "$work" -F "$FIRMWARE_NAME" "$cab" >/dev/null
else
    # bsdtar reads .cab; extract into $work, then locate the firmware by name.
    bsdtar -x -f "$cab" -C "$work"
fi

fw="$(find "$work" -type f -iname "$FIRMWARE_NAME" | head -n1)"
[ -n "$fw" ] || die "the driver package did not contain $FIRMWARE_NAME"

got="$(sha256sum "$fw" | cut -d' ' -f1)"
echo "fetch-firmware: $FIRMWARE_NAME sha256 = $got"
if [ -n "$EXPECTED_SHA256" ]; then
    [ "$got" = "$EXPECTED_SHA256" ] || die "sha256 mismatch (expected $EXPECTED_SHA256)"
    echo "fetch-firmware: hash verified"
else
    echo "fetch-firmware: WARNING — no EXPECTED_SHA256 pinned; firmware is unverified." >&2
    echo "fetch-firmware: pin it (SUNBURST_FW_SHA256 or the constant) after cross-checking xone." >&2
fi

mkdir -p "$(dirname "$DEST")"
cp "$fw" "$DEST"
echo "fetch-firmware: installed → $DEST"

#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-or-later
#
# Phase 0.3 — is the link gigabit, or 100 Mbit?
#
# A 100Mbit PHY caps usable throughput around 80 Mbps, which sits below the
# 70-100 Mbps AV1 target rather than merely tightening it. Worth five minutes
# now against a problem that would otherwise be diagnosed as an encoder or
# pacing bug somewhere in Phase 4.
#
# Usage: tools/check-phy.sh [adb-serial]
#
#
# Why this measures throughput instead of reading the negotiated rate
#
# The obvious check is /sys/class/net/eth0/speed. On a stock Android device that
# is "Permission denied" for the shell user - SELinux - and so are `ip link` and
# `ethtool`. Verified on a SHIELD Android TV (Android 11).
#
# `dumpsys ethernet` does report a number, and it is a trap:
#
#     LinkUpBandwidth>=100000Kbps LinkDnBandwidth>=100000Kbps
#
# That is Android's hardcoded default for the Ethernet transport, not a measured
# PHY rate. It reads as exactly the failure this script exists to detect, on a
# link that is actually gigabit.
#
# So the link is measured instead. Pushing a payload through `adb exec-in` to
# /dev/null keeps storage out of the path, and adb-over-TCP tops out around
# 200 Mbps - which is well above a 100Mbit PHY's ~94 Mbps ceiling and well below
# gigabit. That makes this a reliable *binary* answer and not a rate meter: it
# proves the link is not 100 Mbit, and does not measure how far past it goes.

set -uo pipefail

ADB="${ADB:-adb}"
SERIAL="${1:-}"

# Large enough that connection setup does not dominate, small enough to stay
# quick on the slow answer - a 100Mbit link moves this in about nine seconds.
PAYLOAD_BYTES=$((100 * 1000 * 1000))

# A 100Mbit PHY tops out near 94 Mbps of goodput, so anything comfortably past
# it is gigabit. The gap between the two thresholds is reported as inconclusive
# rather than guessed.
FLOOR_MBPS=120
SUSPECT_MBPS=95

adb_cmd() {
    if [ -n "$SERIAL" ]; then "$ADB" -s "$SERIAL" "$@"; else "$ADB" "$@"; fi
}

if ! command -v "$ADB" >/dev/null 2>&1; then
    echo "adb not found. Set ADB= or put platform-tools on PATH." >&2
    exit 1
fi

devices=$("$ADB" devices | awk 'NR>1 && $2=="device" {print $1}')
if [ -z "$devices" ]; then
    cat >&2 <<'EOF'
No device. Connect over USB, or:
  adb connect <ip>:5555

The Homatics moves its wireless-debugging port between sessions. If connect is
refused, try :5555 before concluding the box is down; ping settles whether it
is actually up.
EOF
    exit 1
fi

if [ -z "$SERIAL" ] && [ "$(echo "$devices" | wc -l)" -gt 1 ]; then
    { echo "More than one device attached; pass a serial:"; echo "$devices" | sed 's/^/  /'; } >&2
    exit 1
fi

echo "== Device =="
printf '  %s %s (Android %s)\n' \
    "$(adb_cmd shell getprop ro.product.manufacturer | tr -d '\r')" \
    "$(adb_cmd shell getprop ro.product.model | tr -d '\r')" \
    "$(adb_cmd shell getprop ro.build.version.release | tr -d '\r')"

# Which interface actually carries traffic. If this says wlan0 the measurement
# below is of wi-fi, and CLAUDE.md specifies 1GbE wired for both clients.
default_iface=$(adb_cmd shell dumpsys ethernet 2>/dev/null \
    | tr -d '\r' | awk -F': ' '/Default interface:/ {print $2; exit}')
echo "  Default ethernet interface: ${default_iface:-none reported}"

if [ -z "$default_iface" ]; then
    echo
    echo "  WARNING: no ethernet interface. If this box is on wi-fi, the number"
    echo "  below is a wi-fi measurement and does not answer Phase 0.3."
fi

echo
echo "== Throughput =="
echo "  Sending $((PAYLOAD_BYTES / 1000 / 1000)) MB to /dev/null on the device..."

tmp=$(mktemp) || { echo "mktemp failed" >&2; exit 1; }
trap 'rm -f "$tmp"' EXIT
# Zeroes, not random: adb does not compress, and /dev/urandom is slow enough at
# this size to become the bottleneck being measured.
head -c "$PAYLOAD_BYTES" /dev/zero > "$tmp"

best=0
for attempt in 1 2 3; do
    start=$(date +%s.%N)
    if ! adb_cmd exec-in "cat > /dev/null" < "$tmp"; then
        echo "  adb exec-in failed on attempt $attempt." >&2
        continue
    fi
    end=$(date +%s.%N)
    mbps=$(awk -v s="$start" -v e="$end" -v b="$PAYLOAD_BYTES" \
        'BEGIN { d = e - s; if (d <= 0) d = 0.001; printf "%.1f", b * 8 / d / 1000000 }')
    printf '  attempt %d: %s Mbps\n' "$attempt" "$mbps"
    # Best of three. Contention only ever makes a link look slower, so the
    # fastest run is the closest to the truth.
    best=$(awk -v a="$best" -v b="$mbps" 'BEGIN { print (b > a) ? b : a }')
done

echo
echo "== Verdict =="
verdict=$(awk -v m="$best" -v floor="$FLOOR_MBPS" -v suspect="$SUSPECT_MBPS" \
    'BEGIN { if (m >= floor) print "ok"; else if (m < suspect) print "bad"; else print "unclear" }')

case "$verdict" in
    ok)
        echo "  $best Mbps -- comfortably past a 100Mbit PHY, so the link is gigabit."
        echo "  This is adb's ceiling, not the link's; it is a yes/no answer, not a rate."
        rc=0
        ;;
    bad)
        echo "  $best Mbps -- consistent with a 100 Mbit PHY."
        echo "  Usable throughput would cap around 80 Mbps, below the 70-100 Mbps AV1"
        echo "  target. Check the cable and the switch port before redesigning anything:"
        echo "  a single bad pair negotiates 100 and looks exactly like this."
        rc=2
        ;;
    *)
        echo "  $best Mbps -- inconclusive, between the two thresholds."
        echo "  Re-run on an idle network. If it stays here, measure with iperf3"
        echo "  rather than trusting adb, which has its own ceiling around 200 Mbps."
        rc=1
        ;;
esac

echo
echo "Record the result in HARDWARE_TESTING.md section 3."
exit $rc

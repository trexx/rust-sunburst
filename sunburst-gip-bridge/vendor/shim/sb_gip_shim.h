// SPDX-License-Identifier: GPL-2.0-or-later
//
// The Sunburst shim over the vendored xow driver: a C seam Rust binds, plus the
// C++ `GipSink` the driver calls in place of xow's per-pad JNI object. This is
// the whole Android-port surface xow_driver_jni.cpp used to be, re-pointed from
// Java to Rust — see vendor/UPSTREAM.md.

#pragma once

#include <cstddef>
#include <cstdint>

class Controller;

// Mirror of the Rust `SbPadEvent` in `src/ffi.rs`. Field order and types must
// match byte for byte (both sides are repr(C)).
struct SbPadEvent {
    uint8_t kind;   // 0 none, 1 connected, 2 disconnected, 3 input
    uint8_t index;  // pad index, 0..3
    uint32_t buttons;
    int16_t lx;
    int16_t ly;
    int16_t rx;
    int16_t ry;
    uint8_t lt;
    uint8_t rt;
    uint8_t battery_present;
    uint8_t battery_level;
    uint8_t battery_flags;
};

// The driver's callbacks, C++-side. The shim implements this and turns the calls
// into queued `SbPadEvent`s; `Dongle`/`WiredController`/`Controller` call these
// instead of the `CallVoidMethod` upcalls they had upstream. `slot` is the
// driver's own slot id — `wcid - 1` for the adapter, `0` wired — which the shim
// maps to a 0..3 pad index.
class GipSink {
public:
    virtual ~GipSink() = default;
    virtual void onControllerAdd(int slot, Controller *controller, short vid, short pid) = 0;
    virtual void onControllerRemove(int slot) = 0;
    virtual void onInput(int slot, uint32_t buttons, uint16_t triggerLeft, uint16_t triggerRight,
                         int16_t stickLeftX, int16_t stickLeftY, int16_t stickRightX,
                         int16_t stickRightY) = 0;
    virtual void onBattery(int slot, uint8_t type, uint8_t level, uint8_t charge) = 0;
    virtual void onAudioRemoved(int slot) = 0;
};

// The C seam `src/ffi.rs` binds. One handle owns one USB device (an adapter
// serving up to four pads, or a single wired pad) and is freed by sb_gip_close.
extern "C" {
void *sb_gip_open_dongle(int fd, const char *firmware_path);
void *sb_gip_open_wired(int fd);
// Writes one event into *out and returns 1, or returns 0 if none is pending.
int sb_gip_poll(void *handle, SbPadEvent *out);
void sb_gip_rumble(void *handle, uint8_t pad, uint16_t low, uint16_t high, uint16_t trig_l,
                   uint16_t trig_r);
bool sb_gip_set_pairing(void *handle, bool on);
// -1 no headset, 0 = 48 kHz mono, 1 = 48 kHz stereo.
int sb_gip_audio_format(void *handle, uint8_t pad);
bool sb_gip_audio_set_enabled(void *handle, uint8_t pad, bool on);
bool sb_gip_audio_set_volume(void *handle, uint8_t pad, uint8_t percent);
void sb_gip_audio_out(void *handle, uint8_t pad, const int16_t *samples, size_t n);
size_t sb_gip_audio_in(void *handle, uint8_t pad, int16_t *out, size_t cap);
// Raw capture (microphone) format code (MS-GIPUSB 3.2.5.1.2, e.g. 0x09 = 24 kHz mono);
// 0 = no microphone, -1 = no such pad. Rust decodes the code to a rate and channel count.
int sb_gip_mic_format(void *handle, uint8_t pad);
void sb_gip_close(void *handle);
}

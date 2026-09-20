// SPDX-License-Identifier: GPL-2.0-or-later
//
// The Sunburst shim: the C seam `src/ffi.rs` binds, implemented over the vendored
// xow driver. Replaces xow_driver_jni.cpp — the driver reports through the C++
// `GipSink` here instead of a per-pad Java object, and this turns those callbacks
// into a thread-safe queue of `SbPadEvent`s the Rust side drains. See
// vendor/UPSTREAM.md.

#include "sb_gip_shim.h"

#include "../xow/controller/controller.h"
#include "../xow/dongle/dongle.h"
#include "../xow/dongle/usb.h"
#include "../xow/wired/wired.h"

#include <array>
#include <cstdint>
#include <deque>
#include <memory>
#include <mutex>

// Sunburst battery-flag bits, mirroring sunburst_core::proto::input::battery_flags.
namespace {
constexpr uint8_t BATT_CHARGING = 1 << 0;
constexpr uint8_t BATT_FULL = 1 << 1;

// Xbox GIP triggers are 10-bit; Sunburst's GamepadState trigger is 0..255.
uint8_t scaleTrigger(uint16_t v) {
    if (v > 1023) {
        v = 1023;
    }
    return static_cast<uint8_t>((static_cast<uint32_t>(v) * 255u) / 1023u);
}

// Keep the queue bounded: input reports arrive faster than a slow poll could
// drain, and a superseded one is worthless. Drop the oldest past this.
constexpr size_t MAX_QUEUED = 512;
}  // namespace

// Turns the driver's callbacks into queued events and routes rumble/audio back to
// the right Controller. One mutex guards everything: the driver's read thread
// pushes, the client thread polls and drives rumble/audio.
class ShimSink : public GipSink {
public:
    void onControllerAdd(int slot, Controller *controller, short, short) override {
        std::lock_guard<std::mutex> lock(mutex);
        if (slot < 0 || slot >= MAX_SLOTS) {
            return;
        }
        int pad = allocatePad();
        if (pad < 0) {
            return;  // already four pads; ignore the extra
        }
        slotToPad[slot] = pad;
        pads[pad].controller = controller;
        pads[pad].batteryPresent = false;
        push({static_cast<uint8_t>(1), static_cast<uint8_t>(pad)});
    }

    void onControllerRemove(int slot) override {
        std::lock_guard<std::mutex> lock(mutex);
        if (slot < 0 || slot >= MAX_SLOTS) {
            return;
        }
        int pad = slotToPad[slot];
        slotToPad[slot] = -1;
        if (pad < 0) {
            return;
        }
        pads[pad] = Pad{};
        push({static_cast<uint8_t>(2), static_cast<uint8_t>(pad)});
    }

    void onInput(int slot, uint32_t buttons, uint16_t triggerLeft, uint16_t triggerRight,
                 int16_t stickLeftX, int16_t stickLeftY, int16_t stickRightX,
                 int16_t stickRightY) override {
        std::lock_guard<std::mutex> lock(mutex);
        int pad = padFor(slot);
        if (pad < 0) {
            return;
        }
        SbPadEvent ev{};
        ev.kind = 3;
        ev.index = static_cast<uint8_t>(pad);
        ev.buttons = buttons;  // XInput XUSB layout == Sunburst's buttons::
        ev.lx = stickLeftX;
        ev.ly = stickLeftY;
        ev.rx = stickRightX;
        ev.ry = stickRightY;
        ev.lt = scaleTrigger(triggerLeft);
        ev.rt = scaleTrigger(triggerRight);
        const Pad &p = pads[pad];
        ev.battery_present = p.batteryPresent ? 1 : 0;
        ev.battery_level = p.batteryLevel;
        ev.battery_flags = p.batteryFlags;
        push(ev);
    }

    void onBattery(int slot, uint8_t type, uint8_t level, uint8_t) override {
        std::lock_guard<std::mutex> lock(mutex);
        int pad = padFor(slot);
        if (pad < 0) {
            return;
        }
        Pad &p = pads[pad];
        p.batteryPresent = true;
        p.batteryLevel = level;
        // type 0 = charging; level 3 = full. Headset/mic bits arrive via audio.
        p.batteryFlags = static_cast<uint8_t>((type == 0 ? BATT_CHARGING : 0) |
                                              (level >= 3 ? BATT_FULL : 0));
    }

    void onAudioRemoved(int) override {
        // Nothing queued: the headset flags are refined in Stage C.
    }

    // Client thread: drain one event.
    int poll(SbPadEvent *out) {
        std::lock_guard<std::mutex> lock(mutex);
        if (queue.empty()) {
            return 0;
        }
        *out = queue.front();
        queue.pop_front();
        return 1;
    }

    // The controller behind a pad index, or null. The caller must hold `mutex`
    // for the whole operation it drives on the returned controller, so a
    // concurrent onControllerRemove cannot free it mid-call.
    Controller *controllerLocked(uint8_t pad) {
        if (pad >= MAX_PADS) {
            return nullptr;
        }
        return pads[pad].controller;
    }

    std::mutex mutex;

private:
    static constexpr int MAX_SLOTS = 16;  // MT_WCID_COUNT
    static constexpr int MAX_PADS = 4;    // sunburst MAX_PADS

    struct Pad {
        Controller *controller = nullptr;
        bool batteryPresent = false;
        uint8_t batteryLevel = 0;
        uint8_t batteryFlags = 0;
    };

    int allocatePad() {
        for (int i = 0; i < MAX_PADS; i++) {
            if (pads[i].controller == nullptr) {
                return i;
            }
        }
        return -1;
    }

    int padFor(int slot) const {
        if (slot < 0 || slot >= MAX_SLOTS) {
            return -1;
        }
        return slotToPad[slot];
    }

    void push(const SbPadEvent &ev) {
        if (queue.size() >= MAX_QUEUED) {
            queue.pop_front();
        }
        queue.push_back(ev);
    }

    std::array<Pad, MAX_PADS> pads{};
    std::array<int, MAX_SLOTS> slotToPad = [] {
        std::array<int, MAX_SLOTS> a{};
        a.fill(-1);
        return a;
    }();
    std::deque<SbPadEvent> queue;
};

// One open device: the sink, plus whichever transport owns it.
struct SbGipHandle {
    ShimSink sink;
    std::unique_ptr<Dongle> dongle;
    std::unique_ptr<WiredController> wired;
};

extern "C" {

void *sb_gip_open_dongle(int fd, const char *firmware_path) {
    try {
        auto handle = std::make_unique<SbGipHandle>();
        auto usb = std::make_unique<UsbDevice>(fd);
        handle->dongle = std::make_unique<Dongle>(std::move(usb), &handle->sink);
        if (!handle->dongle->start(firmware_path ? firmware_path : "")) {
            return nullptr;
        }
        return handle.release();
    } catch (...) {
        return nullptr;
    }
}

void *sb_gip_open_wired(int fd) {
    try {
        auto handle = std::make_unique<SbGipHandle>();
        handle->wired = std::make_unique<WiredController>(fd, &handle->sink);
        if (!handle->wired->start()) {
            return nullptr;
        }
        return handle.release();
    } catch (...) {
        return nullptr;
    }
}

int sb_gip_poll(void *handle, SbPadEvent *out) {
    if (handle == nullptr || out == nullptr) {
        return 0;
    }
    return static_cast<SbGipHandle *>(handle)->sink.poll(out);
}

void sb_gip_rumble(void *handle, uint8_t pad, uint16_t low, uint16_t high, uint16_t trig_l,
                   uint16_t trig_r) {
    if (handle == nullptr) {
        return;
    }
    ShimSink &sink = static_cast<SbGipHandle *>(handle)->sink;
    std::lock_guard<std::mutex> lock(sink.mutex);
    Controller *controller = sink.controllerLocked(pad);
    if (controller != nullptr) {
        controller->inputRumble(static_cast<short>(low), static_cast<short>(high));
        controller->inputRumbleTrigger(static_cast<short>(trig_l), static_cast<short>(trig_r));
    }
}

bool sb_gip_set_pairing(void *handle, bool on) {
    if (handle == nullptr) {
        return false;
    }
    Dongle *dongle = static_cast<SbGipHandle *>(handle)->dongle.get();
    return dongle != nullptr && dongle->setPairing(on);
}

int sb_gip_audio_format(void *handle, uint8_t pad) {
    if (handle == nullptr) {
        return -1;
    }
    ShimSink &sink = static_cast<SbGipHandle *>(handle)->sink;
    std::lock_guard<std::mutex> lock(sink.mutex);
    Controller *c = sink.controllerLocked(pad);
    // Stage C refines this to the pad's negotiated mono/stereo; for now, report
    // stereo when the pad has a usable audio-out, else none.
    return (c != nullptr && c->supportsAudioOut()) ? 1 : -1;
}

bool sb_gip_audio_set_enabled(void *handle, uint8_t pad, bool on) {
    if (handle == nullptr) {
        return false;
    }
    ShimSink &sink = static_cast<SbGipHandle *>(handle)->sink;
    std::lock_guard<std::mutex> lock(sink.mutex);
    Controller *c = sink.controllerLocked(pad);
    return c != nullptr && c->setAudioEnabled(on);
}

bool sb_gip_audio_set_volume(void *handle, uint8_t pad, uint8_t percent) {
    if (handle == nullptr) {
        return false;
    }
    ShimSink &sink = static_cast<SbGipHandle *>(handle)->sink;
    std::lock_guard<std::mutex> lock(sink.mutex);
    Controller *c = sink.controllerLocked(pad);
    return c != nullptr && c->setAudioVolume(percent);
}

void sb_gip_audio_out(void *handle, uint8_t pad, const int16_t *samples, size_t n) {
    if (handle == nullptr || samples == nullptr || n == 0) {
        return;
    }
    ShimSink &sink = static_cast<SbGipHandle *>(handle)->sink;
    std::lock_guard<std::mutex> lock(sink.mutex);
    Controller *c = sink.controllerLocked(pad);
    if (c != nullptr) {
        c->queueAudio(samples, n);
    }
}

size_t sb_gip_audio_in(void *handle, uint8_t pad, int16_t *out, size_t cap) {
    if (handle == nullptr || out == nullptr || cap == 0) {
        return 0;
    }
    ShimSink &sink = static_cast<SbGipHandle *>(handle)->sink;
    std::lock_guard<std::mutex> lock(sink.mutex);
    Controller *c = sink.controllerLocked(pad);
    return c != nullptr ? c->drainCapturedAudio(out, cap) : 0;
}

int sb_gip_mic_format(void *handle, uint8_t pad) {
    if (handle == nullptr) {
        return -1;
    }
    ShimSink &sink = static_cast<SbGipHandle *>(handle)->sink;
    std::lock_guard<std::mutex> lock(sink.mutex);
    Controller *c = sink.controllerLocked(pad);
    // The raw capture format code (e.g. 0x09 = 24 kHz mono); Rust decodes it to a rate/channels.
    // 0 means no microphone; -1 means no such pad.
    return c != nullptr ? static_cast<int>(c->captureFormatCode()) : -1;
}

void sb_gip_close(void *handle) {
    delete static_cast<SbGipHandle *>(handle);
}

}  // extern "C"

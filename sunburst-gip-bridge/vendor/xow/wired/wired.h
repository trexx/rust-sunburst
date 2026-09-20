/*
 * Android port addition - not part of upstream xow.
 *
 * This program is free software; you can redistribute it and/or
 * modify it under the terms of the GNU General Public License
 * as published by the Free Software Foundation; either version 2
 * of the License, or (at your option) any later version.
 */

#pragma once

#include "usb_wired.h"
#include "../controller/controller.h"

#include <atomic>
#include <condition_variable>
#include <memory>
#include <mutex>
#include <thread>
#include <vector>

/*
 * A single cabled GIP controller.
 *
 * The wireless sibling of this is Dongle, which multiplexes up to sixteen clients over MT76 and
 * 802.11 framing. None of that applies here: one cable is one device, and GIP messages sit
 * directly on interface 0's interrupt endpoints with no transport framing around them at all. So
 * this class is the whole transport - claim, read loop, write callback - and everything above it
 * is the GipDevice stack already shared with the adapter.
 *
 * That sharing is the point. Moving cabled pads onto GipDevice is what gives them metadata, the
 * device-state machine, chunk reassembly and above all the security handshake, which MS-GIPUSB
 * 2.2.1.4 makes the gate on the audio sub-device. The existing XboxOneController implements none of
 * those - it sends canned init packets and parses two message types - so it could never reach
 * headphone audio however much isochronous plumbing were added beneath it.
 */
class WiredController : public GipAudioTransport
{
public:
    /*
     * Takes no jobject: nothing here calls back into Java. The GIP layer above does its own
     * callbacks through GipController.registerNative(), and holding a reference here as well would
     * mean the Java object had to exist before the native handle it is constructed from - which it
     * cannot, since the handle is a constructor argument.
     */
    WiredController(int fd, GipSink *sink, uint8_t ledBrightness = 0x14);
    ~WiredController();

    /* Claims the device and starts the read thread. */
    bool start();
    void stop();

    /* @return the GIP device, for the Java layer to drive rumble and audio through */
    Controller *controller() { return gipController.get(); }

    /*
     * GipAudioTransport. A cabled pad's audio is isochronous on its own interface, so it cannot go
     * through the GIP link the way the adapter's does - see the interface's own comment.
     */
    bool enableAudio() override;
    void disableAudio() override;
    bool startStreaming() override;
    bool sendAudio(const uint8_t *samples, size_t length) override;
    uint32_t underruns() const override
    {
        // Both are gaps in the audio, and neither is visible to the other's mechanism
        return audioUnderruns.load(std::memory_order_relaxed)
             + audioIdle.load(std::memory_order_relaxed);
    }
    void resetStats() override
    {
        audioUnderruns.store(0, std::memory_order_relaxed);
        audioIdle.store(0, std::memory_order_relaxed);
    }

private:
    void readPackets();

    /* Reaps completed isochronous transfers; libusb has no callbacks without someone pumping it. */
    void handleAudioEvents();

    /* Called on the event thread when a transfer finishes, to count it and free it for reuse. */
    void audioTransferComplete(libusb_transfer *transfer);

    static void LIBUSB_CALL audioCallback(libusb_transfer *transfer);

    /* Fills a transfer from the ring and puts it back on the wire. */
    bool submitAudioTransfer(libusb_transfer *transfer);

    /*
     * Listens to the capture endpoint, which is where the device says how much render data it
     * wants and, if the headset has a microphone, carries the mic PCM.
     *
     * MS-GIPUSB 3.2.5.1.5 puts the flow rate in the Audio Capture message and calls honouring it
     * "the mechanism GIP devices use to eliminate pops and clicks", and notes a render-only device
     * still sends these "with no payload other than the flow rate to implement rate adaptation".
     * Without submitting here we never hear it, and the device's buffer drifts with nothing able
     * to correct it. The microphone samples after the flow rate are retained for the client (a
     * Sunburst addition; upstream discarded them).
     */
    bool submitCaptureTransfer(libusb_transfer *transfer);

    void captureTransferComplete(libusb_transfer *transfer);

    static void LIBUSB_CALL captureCallback(libusb_transfer *transfer);

    static const int CAPTURE_TRANSFERS = 2;

    std::vector<libusb_transfer *> captureTransfers;
    std::vector<std::vector<uint8_t>> captureBuffers;

    /* Sends a GIP message out interface 0, which is what GipDevice::sendPacket resolves to here. */
    bool sendPacket(const Bytes &data);

    std::unique_ptr<UsbWiredDevice> device;
    std::unique_ptr<Controller> gipController;

    std::atomic<bool> stopThread{false};
    std::thread readThread;

    /*
     * The isochronous ring.
     *
     * Every transfer stays in flight: a completion is refilled from the sample ring and resubmitted
     * immediately, so the endpoint is never left unfed. That is what makes the depth a latency
     * choice again rather than a safety one - audio sits here for the whole queue before it is
     * heard, and nothing is gained by holding more of it.
     *
     * The depth needs to cover the round trip from a completion callback to the resubmission it
     * triggers, and nothing more - but it is also the actuation delay of the pad's rate-adaptation
     * loop, and that is what sized it down from six transfers. A flow-rate request takes the whole
     * queue to reach the wire, so at 24 ms of depth a controller commanding its maximum correction
     * of 4 bytes/ms overshoots its own buffer by ~96 bytes before the first corrected packet
     * arrives. A pad at equilibrium never commands hard corrections and does not care; one adopted
     * mid-stream starts displaced, slams the rails, and 3.2.5.1.5's band gives it no way to ask
     * faster - measured as the flow rate changing 12 times a second for a whole stuttering session
     * against 3.4 in a clean one, both wandering 188-196. Halving the queue halves the overshoot.
     *
     * Twelve packets is still triple the completion-to-resubmit round trip, which is sub-millisecond
     * on this hardware. It was briefly raised to xone's 12 x 8 while chasing degradation that
     * turned out to be the one-shot prime - depth cannot explain a first session that sounds right
     * and a second that does not, since it never changes.
     */
    static const int AUDIO_TRANSFERS = 3;
    static const int AUDIO_PACKETS_PER_TRANSFER = 4;

    // One millisecond of 48 kHz 16-bit stereo, and the GIP message that carries it
    static constexpr size_t AUDIO_FRAGMENT_BYTES = 192;
    static constexpr size_t AUDIO_HEADER_BYTES = 6;

    /*
     * Room for the largest the device may ask for, not the nominal size. It modulates up as well as
     * down - 196 for 48 kHz stereo - and a buffer sized at 192 would clamp the request, leaving
     * adaptation able to slow the stream but never to catch it up.
     */
    static constexpr size_t AUDIO_MAX_FRAGMENT_BYTES = 200;
    static constexpr size_t AUDIO_PACKET_BYTES = AUDIO_MAX_FRAGMENT_BYTES + AUDIO_HEADER_BYTES;

    std::vector<libusb_transfer *> audioTransfers;
    std::vector<std::vector<uint8_t>> audioBuffers;

    // Which transfers are free to fill. Guarded by audioMutex.
    std::vector<int> audioFree;
    std::mutex audioMutex;
    std::condition_variable audioCondition;

    std::atomic<bool> audioRunning{false};

    /*
     * Whether the transfer pools have been put on the wire this session.
     *
     * enableAudio() builds them and startStreaming() submits them, and the gap between the two is
     * deliberate: nothing may touch the isochronous endpoints until the device has reported its
     * volume. See GipAudioTransport::startStreaming().
     */
    std::atomic<bool> streamingStarted{false};

    /*
     * Transfers submitted but not yet completed or cancelled.
     *
     * A cancel is a request, not an event: the transfer is only finished when its callback has
     * fired. Freeing one before that leaves libusb holding a pointer to memory we have given back,
     * which does not fail loudly - it makes the next session behave strangely, which is exactly
     * how this presented. Teardown waits for this to reach zero before freeing anything.
     */
    std::atomic<int> audioOutstanding{0};
    std::atomic<uint32_t> audioUnderruns{0};

    /*
     * Times the queue was completely drained when a buffer arrived, meaning the endpoint had
     * nothing to send and the audio has a hole in it.
     *
     * The per-packet status cannot see this: it reports on packets that were submitted, and this
     * counts the ones that never were. Reporting zero underruns through audibly broken audio is
     * exactly what made the first queue depth look adequate when it was not.
     */
    std::atomic<uint32_t> audioIdle{0};

    /*
     * Whether enough audio has accumulated to start consuming it.
     *
     * A fixed-rate sink fed by a jittery source needs a cushion between them, and pulling without
     * one is worse than pushing was: the ring is drained to empty on every completion, so the next
     * millisecond the host is even slightly late becomes silence, and the buffer can never build
     * back up. Real audio interleaved with silence does not sound like a dropout - it sounds
     * stretched and stuttering, which is exactly what it did.
     *
     * So silence is sent deliberately until there is a cushion, and only then does the ring start
     * being consumed. The pad hears a few milliseconds of nothing at the start of a session, which
     * is the same thing every audio device does when it opens.
     *
     * Cleared again whenever the ring is found completely empty, not only at start-up. The cushion
     * only ever shrinks otherwise - a shortfall is padded and never made back - so a single late
     * millisecond would leave the rest of the session playing from an empty ring. See
     * submitAudioTransfer(), which is where both the arming and the re-arming happen.
     */
    std::atomic<bool> audioPrimed{false};

    // Half the sample ring, so there is as much room to absorb a burst as to ride out a gap
    static constexpr size_t AUDIO_PREFILL_BYTES = 3072;
    std::thread audioEventThread;

    // Sunburst: where the underlying Controller reports; passed to it in start().
    GipSink *sink;

    // Applied to the Controller built in start(), not here: there is no device to talk to yet
    const uint8_t ledBrightness;
};

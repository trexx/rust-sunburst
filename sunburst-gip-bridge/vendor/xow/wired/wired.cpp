/*
 * Android port addition - not part of upstream xow.
 *
 * This program is free software; you can redistribute it and/or
 * modify it under the terms of the GNU General Public License
 * as published by the Free Software Foundation; either version 2
 * of the License, or (at your option) any later version.
 */

#include "wired.h"
#include "../utils/log.h"

#include <functional>
#include <utility>
#include <algorithm>

// GIP audio data messages, which is what the capture endpoint carries
#define GIP_AUDIO_SAMPLES 0x60
#include <vector>

WiredController::WiredController(int fd, GipSink *sink, uint8_t ledBrightness)
    : sink(sink), ledBrightness(ledBrightness)
{
    device = std::make_unique<UsbWiredDevice>(fd);
}

WiredController::~WiredController()
{
    stop();

    // The controller's destructor talks to the device - it sends Set Device State: OFF - so it has
    // to go before the device does. Explicit rather than relying on member declaration order,
    // which would put device first and destroy it last.
    gipController.reset();
    device.reset();
}

bool WiredController::start()
{
    if (device == nullptr || !device->isOpen())
    {
        Log::error("Wired: device did not open");

        return false;
    }

    gipController = std::make_unique<Controller>(
        std::bind(&WiredController::sendPacket, this, std::placeholders::_1),
        ledBrightness
    );

    /*
     * Setting a transport is what stands the sender thread down: audio here is pulled from the ring
     * by the bus, one millisecond per isochronous packet, rather than pushed at the adapter's 8 ms
     * cadence by a thread of our own. Two clocks on one ring is what that would be.
     *
     * The ring's own packet size is left at the default. It sizes the ring - four packets, so
     * 32 ms - and AUDIO_PREFILL_BYTES is half of that; it is not the cadence here, because
     * nothing on this transport waits for a packet's worth of samples.
     */
    gipController->setAudioTransport(this);

    // Sunburst: give the controller its sink + slot (wired is always pad 0)
    // before begin(), so any startup input has somewhere to go.
    gipController->setSink(sink, 0);

    // Android takes this pad back the moment we release it, so switching it off on the way out
    // would leave it dead for whoever picks it up next - us included, on the following connect.
    gipController->setPowerOffOnTeardown(false);

    stopThread = false;
    readThread = std::thread(&WiredController::readPackets, this);

    /*
     * Started by hand, after the reader is up so nothing that comes back is missed.
     *
     * A cabled pad has already been through Arrival by the time we claim it - Android's own driver
     * took it there, which is why it works without us - and MS-GIPUSB 2.2.1 has a device Hello only
     * while in Arrival. So it never announces to us, and everything downstream of the announce
     * would never run. A capture showed exactly that: the pad sent battery status happily and never
     * said hello.
     */
    gipController->begin();

    // Sunburst: announce the single wired pad (slot 0). Its removal rides the
    // handle teardown in sb_gip_close rather than a separate event.
    if (sink) {
        sink->onControllerAdd(0, gipController.get(), 0, 0);
    }

    Log::info("Wired: controller started");

    return true;
}

void WiredController::stop()
{
    if (stopThread)
    {
        return;
    }

    stopThread = true;

    if (readThread.joinable())
    {
        readThread.join();
    }
}

bool WiredController::sendPacket(const Bytes &data)
{
    if (device == nullptr || !device->isOpen())
    {
        return false;
    }

    return device->interruptWrite(data.raw(), data.size());
}

void WiredController::readPackets()
{
    uint8_t buffer[UsbWiredDevice::MAX_TRANSFER_SIZE];

    // Sunburst: the driver makes no Java calls, so the read thread does not
    // attach to a JVM.
    while (!stopThread)
    {
        int transferred = device->interruptRead(buffer, sizeof(buffer));

        // Negative is a real failure - the cable is gone, or the interface was taken away
        if (transferred < 0)
        {
            Log::info("Wired: device is gone; stopping audio");

            /*
             * Stopped here rather than left to teardown, because teardown may be a while coming and
             * the audio transfers do not stop on their own.
             *
             * Every completion resubmits, so once the device is gone the event thread spins
             * resubmitting to a dead handle - which is what "the USB stack cycling its connection
             * once a second" was, and why re-enumeration looked unworkable when it was tried.
             *
             * Safe from this thread: disableAudio() joins the libusb event thread, which is not
             * this one. Tearing the *controller* down from here would be a self-join - the
             * destructor joins this thread - so that stays with the Java side, which learns about
             * the detach from the broadcast.
             */
            disableAudio();

            break;
        }

        // Zero is a read timeout, which is just an idle pad
        if (transferred == 0)
        {
            continue;
        }

        Bytes packet(buffer, buffer + transferred);

        if (!gipController->handlePacket(packet))
        {
            Log::error("Wired: error handling packet");
        }
    }
}


void LIBUSB_CALL WiredController::audioCallback(libusb_transfer *transfer)
{
    static_cast<WiredController *>(transfer->user_data)->audioTransferComplete(transfer);
}

void WiredController::audioTransferComplete(libusb_transfer *transfer)
{
    audioOutstanding.fetch_sub(1, std::memory_order_relaxed);

    /*
     * Isochronous reports a status per packet, not just per transfer, and that is the only place a
     * dropped millisecond is visible - the transfer as a whole still reports COMPLETED. Counting
     * them here is what makes the queue depth above a measured choice rather than a guess.
     */
    for (int i = 0; i < transfer->num_iso_packets; i++)
    {
        if (transfer->iso_packet_desc[i].status != LIBUSB_TRANSFER_COMPLETED)
        {
            audioUnderruns.fetch_add(1, std::memory_order_relaxed);
        }
    }

    // Cancelled transfers are teardown, not failures, and must not go back in the free list
    if (transfer->status == LIBUSB_TRANSFER_CANCELLED || !audioRunning.load())
    {
        return;
    }

    // Straight back out, filled with whatever the ring has. The bus is the clock here: this
    // transfer's slots are going to be transmitted regardless, so the only question is whether
    // they carry audio or silence, and stopping to wait for samples would guarantee silence.
    submitAudioTransfer(transfer);
}

bool WiredController::enableAudio()
{
    if (audioRunning.load())
    {
        return true;
    }

    /*
     * Claimed and selected once per connection, not once per session.
     *
     * Every enable used to release the interface and re-select the alternate setting, and the pad
     * degraded a little more each time - first session clean, second worse, third worse again,
     * cleared only by unplugging. Our own side is measurably innocent: supply matches consumption
     * at 192 bytes per packet with no underruns in the degraded sessions as much as the clean one,
     * so whatever accumulates is in the device, and cycling its streaming interface is the only
     * thing we were doing repeatedly to it.
     *
     * So the interface stays up between sessions and only the sub-device is stopped and started,
     * which is what 2.2.11 actually asks for.
     */
    if (!device->hasAudioInterface() && !device->enableAudioInterface())
    {
        return false;
    }

    /*
     * The render endpoint has to be able to carry a whole GIP audio message. AUDIO_PACKET_BYTES
     * comes from the GIP framing and the endpoint's size comes from the pad, so nothing guarantees
     * they agree - both pads scanned declare 228 out, which is enough, but that is a fact about
     * them rather than about the protocol. Refuse rather than submit transfers the endpoint will
     * truncate, which would be heard as noise rather than reported as an error.
     */
    if (AUDIO_PACKET_BYTES > device->audioMaxPacketSize())
    {
        Log::error("Wired: render endpoint %02x carries %zu bytes, under the %zu a GIP audio "
                   "message needs", device->audioEndpointOut(), device->audioMaxPacketSize(),
                   AUDIO_PACKET_BYTES);

        return false;
    }

    audioUnderruns.store(0, std::memory_order_relaxed);
    audioIdle.store(0, std::memory_order_relaxed);
    audioOutstanding.store(0, std::memory_order_relaxed);
    audioPrimed.store(false, std::memory_order_relaxed);
    audioTransfers.clear();
    audioBuffers.clear();
    audioFree.clear();

    for (int i = 0; i < AUDIO_TRANSFERS; i++)
    {
        libusb_transfer *transfer = libusb_alloc_transfer(AUDIO_PACKETS_PER_TRANSFER);

        if (transfer == nullptr)
        {
            Log::error("Wired: could not allocate an audio transfer");

            disableAudio();

            return false;
        }

        audioTransfers.push_back(transfer);
        audioBuffers.emplace_back(AUDIO_PACKETS_PER_TRANSFER * AUDIO_PACKET_BYTES, 0);
        audioFree.push_back(i);
    }

    for (int i = 0; i < CAPTURE_TRANSFERS; i++)
    {
        libusb_transfer *transfer = libusb_alloc_transfer(AUDIO_PACKETS_PER_TRANSFER);

        if (transfer == nullptr)
        {
            Log::error("Wired: could not allocate a capture transfer");

            disableAudio();

            return false;
        }

        captureTransfers.push_back(transfer);
        // The capture endpoint's own size, which is not the render endpoint's: an Xbox One pad
        // declares 228 both ways where an Elite Series 2 declares 228 out and 64 in.
        captureBuffers.emplace_back(
            device->audioCaptureMaxPacketSize() * AUDIO_PACKETS_PER_TRANSFER, 0);
    }

    audioRunning.store(true);
    streamingStarted.store(false, std::memory_order_relaxed);
    audioEventThread = std::thread(&WiredController::handleAudioEvents, this);

    /*
     * Nothing is submitted here. The pools exist, the event thread is pumping, and the endpoints
     * stay untouched until the device reports its volume - see startStreaming(). This used to
     * submit everything immediately, which put GIP silence messages on the render endpoint while
     * the sub-device was still Idle, before it had been stopped, configured or started. No
     * reference host does that: xone's first URBs are gated on the volume having arrived, and the
     * Windows capture starts render 31 ms after it.
     */
    Log::info("Wired: audio ready, %d transfers of %d packets (%d ms queued), awaiting the device",
              AUDIO_TRANSFERS, AUDIO_PACKETS_PER_TRANSFER,
              AUDIO_TRANSFERS * AUDIO_PACKETS_PER_TRANSFER);

    return true;
}

bool WiredController::startStreaming()
{
    if (!audioRunning.load())
    {
        return false;
    }

    // A resumed session calls this again on pools that are already in flight
    if (streamingStarted.exchange(true))
    {
        return true;
    }

    // Listening before sending, so the first flow rate is in hand as early as possible
    for (libusb_transfer *transfer : captureTransfers)
    {
        submitCaptureTransfer(transfer);
    }

    // Primed with silence until the cushion builds, and all in flight from the first frame so
    // every completion has a transfer to hand straight back
    for (libusb_transfer *transfer : audioTransfers)
    {
        submitAudioTransfer(transfer);
    }

    Log::info("Wired: streaming");

    return true;
}

void WiredController::disableAudio()
{
    if (!audioRunning.exchange(false))
    {
        // Still tear down a half-built pool from a failed enableAudio()
        for (libusb_transfer *transfer : audioTransfers)
        {
            libusb_free_transfer(transfer);
        }

        for (libusb_transfer *transfer : captureTransfers)
        {
            libusb_free_transfer(transfer);
        }

        audioTransfers.clear();
        audioBuffers.clear();
        audioFree.clear();
        captureTransfers.clear();
        captureBuffers.clear();

        return;
    }

    // Wake anything waiting for a free transfer that will now never come
    audioCondition.notify_all();

    for (libusb_transfer *transfer : audioTransfers)
    {
        libusb_cancel_transfer(transfer);
    }

    for (libusb_transfer *transfer : captureTransfers)
    {
        libusb_cancel_transfer(transfer);
    }

    if (audioEventThread.joinable())
    {
        audioEventThread.join();
    }

    for (libusb_transfer *transfer : audioTransfers)
    {
        libusb_free_transfer(transfer);
    }

    for (libusb_transfer *transfer : captureTransfers)
    {
        libusb_free_transfer(transfer);
    }

    audioTransfers.clear();
    audioBuffers.clear();
    audioFree.clear();
    captureTransfers.clear();
    captureBuffers.clear();

    // The interface deliberately stays claimed and on its streaming setting; see enableAudio().
    // It is released when the device goes, in ~UsbWiredDevice.

    Log::info("Wired: audio stopped");
}

void WiredController::handleAudioEvents()
{
    /*
     * libusb delivers completions only while someone is pumping it, and nothing else in this
     * driver does - the read loop is a synchronous transfer. The timeout is what lets this notice
     * teardown rather than blocking in the library.
     */
    while (audioRunning.load())
    {
        // Short, because every completion here is a millisecond the endpoint is waiting on. The
        // thread does nothing else, so the cost is a wakeup rather than work.
        timeval timeout = { 0, 2000 };

        libusb_handle_events_timeout(device->context(), &timeout);
    }

    /*
     * Keeps pumping until every submitted transfer has come back. Teardown cancels them, but a
     * cancel only asks - the transfer is finished when its callback fires, and until then libusb
     * still owns the buffer. Leaving before that means they are freed underneath it.
     *
     * Bounded so a transfer that never completes cannot hold teardown open; a second is far longer
     * than a cancellation takes and still shorter than a user notices.
     */
    for (int i = 0; i < 500 && audioOutstanding.load(std::memory_order_relaxed) > 0; i++)
    {
        timeval timeout = { 0, 2000 };

        libusb_handle_events_timeout(device->context(), &timeout);
    }

    int stranded = audioOutstanding.load(std::memory_order_relaxed);

    if (stranded > 0)
    {
        Log::error("Wired: %d audio transfers never completed", stranded);
    }
}

bool WiredController::sendAudio(const uint8_t *samples, size_t length)
{
    /*
     * Never called: this transport pulls. Controller only pushes through here when it is the
     * clock, and it stands its sender thread down when a transport is set.
     */
    return false;
}

bool WiredController::submitAudioTransfer(libusb_transfer *transfer)
{
    if (!audioRunning.load())
    {
        return false;
    }

    // Which buffer belongs to this transfer; the pool is small, so a scan beats threading an index
    // through user_data alongside the object pointer.
    int index = -1;

    for (size_t i = 0; i < audioTransfers.size(); i++)
    {
        if (audioTransfers[i] == transfer)
        {
            index = static_cast<int>(i);

            break;
        }
    }

    if (index < 0)
    {
        return false;
    }

    uint8_t *buffer = audioBuffers[index].data();
    uint8_t fragment[AUDIO_MAX_FRAGMENT_BYTES];

    /*
     * What the device asked for, not what we assume. 3.2.5.1.5 has it nudge this by a sample per
     * channel per millisecond according to its own buffering, and honouring it is how the two
     * clocks are reconciled - sending a fixed size instead lets its buffer drift until the audio
     * stretches, which is what it did.
     */
    const size_t fragmentBytes = std::min(gipController->audioRenderBytes(),
                                          AUDIO_MAX_FRAGMENT_BYTES);
    const size_t packetBytes = AUDIO_HEADER_BYTES + fragmentBytes;

    /*
     * One GIP message per millisecond, each with its own sequence number, at the packet stride the
     * endpoint expects - the device reads each isochronous packet as a whole message, so the header
     * cannot be written once for the buffer.
     */
    /*
     * Held off until there is a cushion. Consuming from an empty ring produces silence and keeps
     * the ring empty, so without this the stream never recovers from its own start.
     */
    if (!audioPrimed.load(std::memory_order_relaxed)
        && gipController->audioBuffered() >= AUDIO_PREFILL_BYTES)
    {
        audioPrimed.store(true, std::memory_order_relaxed);

        Log::info("Wired: audio buffer primed, streaming");
    }

    for (int i = 0; i < AUDIO_PACKETS_PER_TRANSFER; i++)
    {
        if (!audioPrimed.load(std::memory_order_relaxed))
        {
            // Deliberate silence while filling, not a shortfall, so it is not counted as one
            std::fill(fragment, fragment + sizeof(fragment), 0);
        }
        else if (gipController->drainAudio(fragment, fragmentBytes) == 0)
        {
            audioIdle.fetch_add(1, std::memory_order_relaxed);

            /*
             * A completely empty ring is the cushion gone, so rebuild it rather than carrying on
             * without one. Priming was a start-up step only, and it cannot be: the cushion ratchets
             * downwards. drainAudio() takes what is there and pads the rest, so a shortfall
             * shrinks the cushion permanently and nothing ever puts it back - one late
             * millisecond and every later one is served from a ring hovering at empty. That is
             * heard as audio interleaved with silence, which does not sound like a dropout; it
             * sounds gapped and slowed down.
             *
             * It matters most between sessions. Disabling audio leaves the stream up carrying
             * silence, deliberately - the device is configured once and never renegotiated - so the
             * ring is simply not fed and drains to empty. Re-enabling then resumes into the running
             * stream without going near enableAudio(), which is the only other place this is
             * cleared, and the next session plays from an empty ring for its whole length.
             *
             * Re-arming here costs the AUDIO_PREFILL_BYTES of deliberate silence that a start
             * already costs, which is the same trade made there and the one every audio device
             * makes when it opens.
             */
            audioPrimed.store(false, std::memory_order_relaxed);
        }

        /*
         * Packed contiguously, at the size actually being sent rather than at the maximum stride.
         * The kernel lays isochronous packets out one after another by their declared lengths, so
         * a gap between them is not padding - every packet after the first would be read from the
         * wrong offset, which is heard as noise.
         */
        gipController->encodeAudioFragment(fragment, fragmentBytes,
                                           buffer + i * packetBytes);
    }

    libusb_fill_iso_transfer(transfer, device->deviceHandle(),
                             device->audioEndpointOut(),
                             buffer,
                             static_cast<int>(AUDIO_PACKETS_PER_TRANSFER * packetBytes),
                             AUDIO_PACKETS_PER_TRANSFER, audioCallback, this, 1000);

    // Every packet in a transfer carries the same size, since the rate is read once per transfer
    libusb_set_iso_packet_lengths(transfer, static_cast<unsigned int>(packetBytes));

    audioOutstanding.fetch_add(1, std::memory_order_relaxed);

    int error = libusb_submit_transfer(transfer);

    if (error)
    {
        audioOutstanding.fetch_sub(1, std::memory_order_relaxed);

        Log::error("Wired: audio submit failed: %s", libusb_error_name(error));

        return false;
    }

    return true;
}

void LIBUSB_CALL WiredController::captureCallback(libusb_transfer *transfer)
{
    static_cast<WiredController *>(transfer->user_data)->captureTransferComplete(transfer);
}

void WiredController::captureTransferComplete(libusb_transfer *transfer)
{
    audioOutstanding.fetch_sub(1, std::memory_order_relaxed);

    if (transfer->status == LIBUSB_TRANSFER_CANCELLED || !audioRunning.load())
    {
        return;
    }

    /*
     * Decoded here rather than through handlePacket(), which must not be called from this thread.
     *
     * That parser owns the chunk reassembly buffer and the sequence counters and is only ever
     * entered from the interrupt read thread; calling it from the libusb event thread as well put
     * two threads inside it at once and corrupted its state, which crashed the process in a
     * completely unrelated function.
     *
     * The flow rate is four bytes into a message whose header is fixed width for the lengths a
     * capture packet uses; the microphone PCM (if any) follows it, and is retained here for the
     * client to drain (a Sunburst addition - upstream discarded it).
     */
    for (int i = 0; i < transfer->num_iso_packets; i++)
    {
        const libusb_iso_packet_descriptor &packet = transfer->iso_packet_desc[i];

        if (packet.status != LIBUSB_TRANSFER_COMPLETED || packet.actual_length < 6)
        {
            continue;
        }

        const uint8_t *data = libusb_get_iso_packet_buffer_simple(transfer, i);

        // Command, then flags carrying the device id in their low nibble
        if (data[0] != GIP_AUDIO_SAMPLES || (data[1] & 0x0f) == 0)
        {
            continue;
        }

        /*
         * Header is command, flags and sequence, then a length varint - one byte below 128, two
         * above. A capture message is short, so its length fits in one, but the second is checked
         * rather than assumed.
         */
        size_t header = (data[3] & 0x80) ? 5 : 4;

        if (packet.actual_length < header + sizeof(uint16_t))
        {
            continue;
        }

        uint16_t flowRate = static_cast<uint16_t>(data[header] | (data[header + 1] << 8));

        gipController->audioFlowRateReported(flowRate);

        // The microphone PCM follows the flow rate (MS-GIPUSB 3.2.5.1.4); retain it for the
        // client to drain. Empty for a headset with no mic (flow-rate-only capture messages).
        const size_t pcmOffset = header + sizeof(uint16_t);
        if (static_cast<size_t>(packet.actual_length) > pcmOffset)
        {
            gipController->captureAudioReceived(data + pcmOffset,
                                                packet.actual_length - pcmOffset);
        }
    }

    submitCaptureTransfer(transfer);
}

bool WiredController::submitCaptureTransfer(libusb_transfer *transfer)
{
    if (!audioRunning.load())
    {
        return false;
    }

    int index = -1;

    for (size_t i = 0; i < captureTransfers.size(); i++)
    {
        if (captureTransfers[i] == transfer)
        {
            index = static_cast<int>(i);

            break;
        }
    }

    if (index < 0)
    {
        return false;
    }

    const int packetSize = static_cast<int>(device->audioCaptureMaxPacketSize());

    libusb_fill_iso_transfer(transfer, device->deviceHandle(),
                             device->audioEndpointIn(),
                             captureBuffers[index].data(),
                             packetSize * AUDIO_PACKETS_PER_TRANSFER,
                             AUDIO_PACKETS_PER_TRANSFER, captureCallback, this, 1000);

    libusb_set_iso_packet_lengths(transfer, packetSize);

    audioOutstanding.fetch_add(1, std::memory_order_relaxed);

    int error = libusb_submit_transfer(transfer);

    if (error)
    {
        audioOutstanding.fetch_sub(1, std::memory_order_relaxed);

        Log::error("Wired: capture submit failed: %s", libusb_error_name(error));

        return false;
    }

    return true;
}

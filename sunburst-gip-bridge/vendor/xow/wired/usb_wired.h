/*
 * Android port addition - not part of upstream xow.
 *
 * This program is free software; you can redistribute it and/or
 * modify it under the terms of the GNU General Public License
 * as published by the Free Software Foundation; either version 2
 * of the License, or (at your option) any later version.
 */

#pragma once

#include "../utils/bytes.h"

#include <libusb/libusb.h>
#include <cstdint>

/*
 * A cabled GIP device, reached over libusb.
 *
 * Separate from dongle/usb.cpp's UsbDevice rather than sharing it, because that class is shaped
 * for the adapter: it resets the device, sets the configuration, claims interface 0 unconditionally
 * and offers only bulk transfers. A pad wants none of the first three - Android has already
 * configured it, and resetting a controller mid-session drops the input Android is still using -
 * and none of its transfers are bulk.
 *
 * MS-GIPUSB 2.2.12 fixes which interface does what: 0 carries GIP, 1 alt 1 carries audio. It does
 * not fix the endpoint addresses on them, and pads disagree. Two scanned here:
 *
 *   Xbox One pad, 045e:02dd
 *     interface 0 alt 0   ep 0x01 intr 64 @4ms,  ep 0x81 intr 64 @4ms
 *     interface 1 alt 1   ep 0x02 isoc 228 @1ms, ep 0x82 isoc 228 @1ms
 *
 *   Xbox Elite Series 2, 045e:0b00 - every address one higher
 *     interface 0 alt 0   ep 0x02 intr 64 @4ms,  ep 0x82 intr 64 @4ms
 *     interface 1 alt 1   ep 0x03 isoc 228 @1ms, ep 0x83 isoc 64 @1ms
 *
 * The Elite is the one that matches the specification: 2.2.12 gives 228 out and 64 in, and the
 * Xbox One pad's 228 both ways is the outlier. Neither can be assumed, so both are read from the
 * active configuration descriptor at open - see resolveEndpoints().
 *
 * These were hardcoded to the Xbox One pad's addresses, which meant an Elite was written to
 * endpoint 0x01, an endpoint it does not have: LIBUSB_ERROR_IO on the first transfer, before a
 * single GIP byte moved, and input dead with it because Java had already detached the kernel
 * driver from interface 0. The audio constants were worse than useless on that pad - 0x02 is its
 * *GIP interrupt* endpoint, so audio would have been submitted onto the endpoint carrying input.
 *
 * So GIP messages - input, rumble, metadata, the security handshake, audio control - all travel on
 * interface 0's interrupt endpoints, and only the audio samples themselves are isochronous. That
 * split is why the whole GIP stack has to be reachable here before audio is: the handshake that
 * gates the audio sub-device runs entirely on interface 0.
 */
class UsbWiredDevice
{
public:
    // Which interface does what is fixed by 2.2.12; only the endpoints on them vary by pad
    static const int GIP_INTERFACE = 0;

    /*
     * The Command data class MTU is 64 bytes, and a fragment never exceeds one interrupt packet.
     *
     * Still a compile-time constant because readPackets() sizes a stack buffer with it. That makes
     * it an assumption about every pad, so resolveEndpoints() refuses a device whose interrupt
     * endpoint declares more than this rather than reading into a buffer too small to hold it.
     */
    static const size_t MAX_TRANSFER_SIZE = 64;

    /* Interface 0's interrupt endpoints, read from the descriptor at open. */
    uint8_t endpointIn() const { return gipIn; }
    uint8_t endpointOut() const { return gipOut; }

    /*
     * The caller must already have claimed interface 0 on this fd, and must keep it claimed for
     * the life of this object.
     *
     * Claiming is left to Java deliberately. Android's own driver holds a cabled pad's interface,
     * and the only thing that detaches it is UsbDeviceConnection.claimInterface(iface, true) -
     * which is what every other cabled driver here does. libusb_claim_interface on the wrapped fd
     * would just fail with EBUSY against the kernel driver, so this does not attempt it: one
     * owner, and it is the one with the API that can win.
     */
    explicit UsbWiredDevice(int fd);
    ~UsbWiredDevice();

    /* @return whether the device opened */
    bool isOpen() const { return handle != nullptr; }

    /*
     * @return bytes read, 0 on timeout, negative on error. A timeout is not a failure: a pad with
     *         nothing to say is silent, and the read loop simply goes round again.
     */
    int interruptRead(uint8_t *buffer, size_t length);

    bool interruptWrite(const uint8_t *data, size_t length);

    /*
     * Audio lives on its own interface, and only on its second alternate setting: alt 0 has no
     * endpoints at all, so the isochronous pair does not exist until this is called (MS-GIPUSB
     * 2.2.12, confirmed by a descriptor scan). This is exactly what xone's
     * xone_wired_enable_audio() does.
     *
     * Claimed here rather than from Java, unlike interface 0. Nothing else holds this one - Android
     * has no driver for a GIP audio interface - so there is no kernel driver to lose to, and doing
     * it through libusb keeps the alt setting with the library that will be submitting to it.
     *
     * @return whether the interface was claimed and the alternate setting selected
     */
    bool enableAudioInterface();
    void disableAudioInterface();

    /* @return whether the audio interface is currently claimed and on its streaming alt setting */
    bool hasAudioInterface() const { return audioClaimed; }

    libusb_device_handle *deviceHandle() const { return handle; }
    libusb_context *context() const { return ctx; }

    // Interface 1 alt 1 is where audio lives; its endpoints and packet size come from the device
    static const int AUDIO_INTERFACE = 1;
    static const int AUDIO_ALT_SETTING = 1;

    uint8_t audioEndpointOut() const { return audioOut; }
    uint8_t audioEndpointIn() const { return audioIn; }

    /*
     * The render endpoint's wMaxPacketSize, which is what an isochronous packet may carry. Sized
     * from the device rather than assumed: 228 on both pads scanned, but the capture endpoint
     * differs between them, so neither number is a property of "a GIP pad".
     */
    size_t audioMaxPacketSize() const { return audioOutMaxPacket; }
    size_t audioCaptureMaxPacketSize() const { return audioInMaxPacket; }

    /*
     * @return whether this pad declared the isochronous pair at all. A pad without them is a pad
     *         without headphone audio, which is not a failure - it just has nothing to enable.
     */
    bool hasAudioEndpoints() const { return audioOut != 0 && audioIn != 0; }

private:
    /*
     * Reads the endpoint addresses and packet sizes out of the active configuration.
     *
     * Runs once, at open. Interfaces are matched on their descriptor's own interface number and
     * alternate setting rather than on position in the array: position happens to agree on both
     * pads scanned, and that is not something to rely on.
     *
     * @return whether interface 0 yielded an interrupt pair. False is fatal - without it there is
     *         no way to talk to the pad at all, and guessing is what this replaces. The audio pair
     *         is optional and its absence only disables audio.
     */
    bool resolveEndpoints();

    libusb_context *ctx = nullptr;
    libusb_device_handle *handle = nullptr;
    bool audioClaimed = false;

    // Zero means "not found"; no USB endpoint address is zero, since bit 7 is the direction
    uint8_t gipIn = 0;
    uint8_t gipOut = 0;
    uint8_t audioOut = 0;
    uint8_t audioIn = 0;
    size_t audioOutMaxPacket = 0;
    size_t audioInMaxPacket = 0;
};

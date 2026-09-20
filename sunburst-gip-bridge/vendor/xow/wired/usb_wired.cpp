/*
 * Android port addition - not part of upstream xow.
 *
 * This program is free software; you can redistribute it and/or
 * modify it under the terms of the GNU General Public License
 * as published by the Free Software Foundation; either version 2
 * of the License, or (at your option) any later version.
 */

#include "usb_wired.h"
#include "../utils/log.h"

/*
 * A read timeout has to be long enough not to spin and short enough that the loop notices it has
 * been asked to stop. Input arrives every 4 ms when anything is happening and not at all when
 * nothing is, so this is a stop-responsiveness figure rather than a data one.
 */
#define USB_TIMEOUT_READ 500
#define USB_TIMEOUT_WRITE 1000

// Interface 0 carries GIP. Interface 1 is audio and is claimed separately, only if audio is wanted.
#define GIP_INTERFACE 0

UsbWiredDevice::UsbWiredDevice(int fd)
{
    /*
     * Android owns enumeration, so libusb must not go looking for devices itself - it has no
     * permission to, and on Android it would find nothing. The fd from UsbDeviceConnection is the
     * only way in, which is the same handoff dongle/usb.cpp uses.
     */
    int error = libusb_set_option(nullptr, LIBUSB_OPTION_NO_DEVICE_DISCOVERY, nullptr);

    if (error != LIBUSB_SUCCESS)
    {
        Log::error("Wired: libusb_set_option failed: %s", libusb_error_name(error));

        return;
    }

    error = libusb_init(&ctx);

    if (error < 0)
    {
        Log::error("Wired: libusb_init failed: %s", libusb_error_name(error));

        return;
    }

    error = libusb_wrap_sys_device(ctx, (intptr_t)fd, &handle);

    if (error < 0 || handle == nullptr)
    {
        Log::error("Wired: libusb_wrap_sys_device failed: %s", libusb_error_name(error));

        handle = nullptr;

        return;
    }

    /*
     * No reset, no set_configuration and no claim, unlike the dongle path. Android has already
     * configured the device and the Java layer has already claimed interface 0 off the kernel
     * driver on this same fd; resetting or re-claiming here would either fail or drop the device
     * out from under the claim that just succeeded.
     */
    Log::info("Wired: device opened");

    /*
     * Refuse the device rather than write to a guessed endpoint.
     *
     * Closing here leaves isOpen() false, so WiredController::start() fails, the Java factory
     * destroys the transport and releases interface 0, and the kernel driver reattaches. The pad
     * then works exactly as it does with this driver disabled - no headphone audio, but input
     * intact. That is strictly better than the alternative, which is what a wrong endpoint address
     * already produced on hardware: the claim succeeds, every transfer fails with
     * LIBUSB_ERROR_IO, and the pad is left claimed by a driver that cannot drive it.
     *
     * libusb_close on a wrapped device does not close the caller's file descriptor - Android still
     * owns it - so this is safe to do behind the Java layer's back.
     */
    if (!resolveEndpoints())
    {
        libusb_close(handle);

        handle = nullptr;
    }
}

/*
 * Endpoint addresses are a property of the pad, not of the protocol. See the class comment for the
 * two layouts this has been run against and what assuming one of them cost.
 */
bool UsbWiredDevice::resolveEndpoints()
{
    libusb_config_descriptor *config = nullptr;

    int error = libusb_get_active_config_descriptor(libusb_get_device(handle), &config);

    if (error || config == nullptr)
    {
        Log::error("Wired: could not read the configuration descriptor: %s",
                   libusb_error_name(error));

        return false;
    }

    for (uint8_t i = 0; i < config->bNumInterfaces; i++)
    {
        const libusb_interface &iface = config->interface[i];

        for (int alt = 0; alt < iface.num_altsetting; alt++)
        {
            const libusb_interface_descriptor &setting = iface.altsetting[alt];

            // Matched on what the descriptor says it is, not on where it sits in the array
            const bool isGip = setting.bInterfaceNumber == GIP_INTERFACE
                               && setting.bAlternateSetting == 0;
            const bool isAudio = setting.bInterfaceNumber == AUDIO_INTERFACE
                                 && setting.bAlternateSetting == AUDIO_ALT_SETTING;

            if (!isGip && !isAudio)
            {
                continue;
            }

            for (uint8_t e = 0; e < setting.bNumEndpoints; e++)
            {
                const libusb_endpoint_descriptor &endpoint = setting.endpoint[e];

                const uint8_t type = endpoint.bmAttributes & LIBUSB_TRANSFER_TYPE_MASK;
                const bool in = (endpoint.bEndpointAddress & LIBUSB_ENDPOINT_DIR_MASK)
                                == LIBUSB_ENDPOINT_IN;

                if (isGip && type == LIBUSB_TRANSFER_TYPE_INTERRUPT)
                {
                    /*
                     * The size check is the one that keeps readPackets()'s stack buffer honest.
                     * Both pads scanned declare exactly MAX_TRANSFER_SIZE, which is the Command
                     * class MTU, so a pad declaring more is unknown territory rather than a
                     * variation to accommodate silently.
                     */
                    if (endpoint.wMaxPacketSize > MAX_TRANSFER_SIZE)
                    {
                        Log::error("Wired: interrupt endpoint %02x wants %u bytes, over the %zu "
                                   "this driver reads into",
                                   endpoint.bEndpointAddress, endpoint.wMaxPacketSize,
                                   MAX_TRANSFER_SIZE);

                        libusb_free_config_descriptor(config);

                        return false;
                    }

                    (in ? gipIn : gipOut) = endpoint.bEndpointAddress;
                }
                else if (isAudio && type == LIBUSB_TRANSFER_TYPE_ISOCHRONOUS)
                {
                    (in ? audioIn : audioOut) = endpoint.bEndpointAddress;
                    (in ? audioInMaxPacket : audioOutMaxPacket) = endpoint.wMaxPacketSize;
                }
            }
        }
    }

    libusb_free_config_descriptor(config);

    if (gipIn == 0 || gipOut == 0)
    {
        Log::error("Wired: interface %d has no interrupt pair (in %02x, out %02x); refusing the "
                   "pad rather than guessing", GIP_INTERFACE, gipIn, gipOut);

        return false;
    }

    /*
     * Logged unconditionally rather than in debug builds only. A pad that misbehaves on a cable is
     * diagnosed from this line first, and it is two printf arguments once per connect.
     */
    Log::info("Wired: GIP on %02x/%02x, audio on %02x/%02x (%zu/%zu bytes)",
              gipOut, gipIn, audioOut, audioIn, audioOutMaxPacket, audioInMaxPacket);

    return true;
}

UsbWiredDevice::~UsbWiredDevice()
{
    if (handle == nullptr)
    {
        if (ctx != nullptr)
        {
            libusb_exit(ctx);
        }

        return;
    }

    // Interface 0 is the Java layer's to release, for the same reason it is Java's to claim.
    // Interface 1 is ours.
    disableAudioInterface();

    libusb_close(handle);

    if (ctx != nullptr)
    {
        libusb_exit(ctx);
    }
}

int UsbWiredDevice::interruptRead(uint8_t *buffer, size_t length)
{
    int transferred = 0;

    int error = libusb_interrupt_transfer(handle, gipIn, buffer,
                                          static_cast<int>(length), &transferred,
                                          USB_TIMEOUT_READ);

    // Silence is the normal state of an idle pad, so a timeout returns "nothing" rather than an
    // error the caller would have to special-case.
    if (error == LIBUSB_ERROR_TIMEOUT)
    {
        return 0;
    }

    if (error)
    {
        Log::error("Wired: interrupt read failed: %s", libusb_error_name(error));

        return -1;
    }

    return transferred;
}

bool UsbWiredDevice::interruptWrite(const uint8_t *data, size_t length)
{
    int transferred = 0;

    int error = libusb_interrupt_transfer(handle, gipOut, const_cast<uint8_t *>(data),
                                          static_cast<int>(length), &transferred,
                                          USB_TIMEOUT_WRITE);

    if (error)
    {
        Log::error("Wired: interrupt write failed: %s", libusb_error_name(error));

        return false;
    }

    return transferred == static_cast<int>(length);
}

bool UsbWiredDevice::enableAudioInterface()
{
    if (handle == nullptr)
    {
        return false;
    }

    if (audioClaimed)
    {
        return true;
    }

    /*
     * A pad that declared no isochronous pair has no headphone jack to reach, and claiming its
     * audio interface would succeed while leaving nothing to submit to. Checked here rather than
     * left to fail at the first transfer, which is the shape of failure this whole change exists
     * to remove.
     */
    if (!hasAudioEndpoints())
    {
        Log::error("Wired: no isochronous pair on interface %d; this pad has no audio",
                   AUDIO_INTERFACE);

        return false;
    }

    int error = libusb_claim_interface(handle, AUDIO_INTERFACE);

    if (error)
    {
        Log::error("Wired: could not claim the audio interface: %s", libusb_error_name(error));

        return false;
    }

    /*
     * Down to the idle setting first, so a session starts from a known state rather than whatever
     * the last one left. A clean toggle-off does this on the way out, but a killed process does
     * not - and then the pad is still on the streaming setting with its audio device started, and
     * the next session builds on top of that. Re-selecting resets the endpoint, which is otherwise
     * only fixed by unplugging the pad.
     *
     * Failure here is ignored: it means the interface was already idle, which is the state wanted.
     */
    libusb_set_interface_alt_setting(handle, AUDIO_INTERFACE, 0);

    /*
     * The endpoints only exist on alt setting 1; alt 0 is the idle setting and declares none. Until
     * this succeeds there is nothing to submit to, so a failure here is the end of audio rather
     * than something to carry on past.
     */
    error = libusb_set_interface_alt_setting(handle, AUDIO_INTERFACE, AUDIO_ALT_SETTING);

    if (error)
    {
        Log::error("Wired: could not select the audio alt setting: %s", libusb_error_name(error));

        libusb_release_interface(handle, AUDIO_INTERFACE);

        return false;
    }

    audioClaimed = true;

    Log::info("Wired: audio interface claimed, alt setting %d selected", AUDIO_ALT_SETTING);

    return true;
}

void UsbWiredDevice::disableAudioInterface()
{
    if (handle == nullptr || !audioClaimed)
    {
        return;
    }

    audioClaimed = false;

    // Back to the setting with no endpoints, so the device stops expecting a stream
    int error = libusb_set_interface_alt_setting(handle, AUDIO_INTERFACE, 0);

    if (error)
    {
        Log::error("Wired: could not release the audio alt setting: %s", libusb_error_name(error));
    }

    error = libusb_release_interface(handle, AUDIO_INTERFACE);

    if (error)
    {
        Log::error("Wired: could not release the audio interface: %s", libusb_error_name(error));
    }
}

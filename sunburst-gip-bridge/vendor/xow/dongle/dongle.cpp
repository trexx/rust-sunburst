/*
 * Copyright (C) 2019 Medusalix
 *
 * This program is free software; you can redistribute it and/or
 * modify it under the terms of the GNU General Public License
 * as published by the Free Software Foundation; either version 2
 * of the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program; if not, write to the Free Software
 * Foundation, Inc., 51 Franklin Street, Fifth Floor, Boston, MA  02110-1301, USA.
 */


#include <memory>
#include <cassert>

#include "dongle.h"
#include "../utils/log.h"

Dongle::Dongle(
    std::unique_ptr<UsbDevice> usbDevice,
    GipSink *sink,
    uint8_t ledBrightness
) : Mt76(std::move(usbDevice)), stopThreads(false), sink(sink),
    ledBrightness(ledBrightness)
{
    Log::info("Dongle initialized");
}

bool Dongle::start(std::string firmwarePath) {

    if(!init(firmwarePath)) {
        return false;
    }

    threads.emplace_back(
            &Dongle::readBulkPackets,
            this,
            MT_EP_READ
    );
    threads.emplace_back(
            &Dongle::readBulkPackets,
            this,
            MT_EP_READ_PACKET
    );
    return true;
}

bool Dongle::setPairing(bool enable) {
    std::lock_guard<std::mutex> lock(pairingMutex);

    return setPairingStatus(enable);
}

void Dongle::stop() {
    if(stopThreads) {
        return;
    }
    stopThreads = true;
    // Wait for all threads to shut down
    for (std::thread &thread : threads)
    {
        if (thread.joinable())
        {
            thread.join();
        }
    }
}

Dongle::~Dongle()
{
    stop();

    // Sunburst: no Java reference to release.
}

void Dongle::handleControllerConnect(Bytes address)
{
    Log::debug("handleControllerConnect");
    std::lock_guard<std::mutex> lock(controllerMutex);

    // A controller retransmits its association request if it doesn't see the response quickly
    // enough. associateClient() keys purely off a free-slot bitmask and never looks at the
    // address, so without this the retransmission takes a second WCID and builds a second
    // Controller for one physical pad - which then shows up twice in Moonlight, with only one
    // of the two receiving input. There are 16 slots, so repeated retries exhaust them too.
    for (uint8_t i = 0; i < MT_WCID_COUNT; i++)
    {
        if (controllers[i] && clientAddresses[i] == address)
        {
            Log::debug("Ignoring duplicate association for controller '%d'", i + 1);

            return;
        }
    }

    uint8_t wcid = associateClient(address);

    if (wcid == 0)
    {
        Log::error("Failed to associate controller");

        return;
    }

    GipDevice::SendPacket sendPacket = std::bind(
        &Dongle::sendClientPacket,
        this,
        wcid,
        address,
        std::placeholders::_1
    );
    auto uptr = std::make_unique<Controller>(sendPacket, ledBrightness);
    Controller *rawptr = uptr.get();
    controllers[wcid - 1] = std::move(uptr);
    clientAddresses[wcid - 1] = address;
    notifyControllerAdd(wcid - 1, rawptr, 0xdead, 0xbeef);
    Log::info("Controller '%d' connected", wcid);
}

void Dongle::notifyControllerAdd(int id, Controller *controller, short vid, short pid) {
    // Give the controller its sink + slot, then announce it. Called on the read
    // thread; the shim's queue is the thread-safe boundary.
    controller->setSink(sink, id);
    if (sink) {
        sink->onControllerAdd(id, controller, vid, pid);
    }
}

void Dongle::handleControllerDisconnect(uint8_t wcid)
{
    // Ignore invalid WCIDs
    if (wcid == 0 || wcid > MT_WCID_COUNT)
    {
        return;
    }

    std::lock_guard<std::mutex> lock(controllerMutex);

    // Ignore unconnected controllers
    if (!controllers[wcid - 1])
    {
        return;
    }

    notifyControllerRemove(wcid - 1);

    controllers[wcid - 1].reset();

    // Release the address alongside the slot, so the same pad can associate again later
    clientAddresses[wcid - 1] = Bytes();

    if (!removeClient(wcid))
    {
        Log::error("Failed to remove controller");

        return;
    }

    Log::info("Controller '%d' disconnected", wcid);
}

void Dongle::notifyControllerRemove(int id) {
    if (sink) {
        sink->onControllerRemove(id);
    }
}

void Dongle::handleControllerPair(Bytes address, const Bytes &packet)
{
    // Ignore invalid packets
    if (packet.size() < sizeof(ReservedFrame))
    {
        return;
    }

    const ReservedFrame *frame = packet.toStruct<ReservedFrame>();

    // Type 0x01 is for pairing requests
    if (frame->type != 0x01)
    {
        return;
    }

    if (!pairClient(address))
    {
        Log::error("Failed to pair controller");

        return;
    }

    if (!setPairing(false))
    {
        Log::error("Failed to disable pairing");

        return;
    }

    // Guarded at the call site, not left to Log::debug(): its release body is empty but its
    // arguments are still evaluated, and formatBytes() builds a string. The only call site in
    // this driver where a debug line costs anything when nothing is listening.
#ifdef _DEBUG
    Log::debug(
        "Controller paired: %s",
        Log::formatBytes(address).c_str()
    );
#endif
}

void Dongle::handleControllerPacket(uint8_t wcid, const Bytes &packet)
{
    // Invalid WCID
    if (wcid == 0 || wcid > MT_WCID_COUNT)
    {
        return;
    }

    // Ignore invalid or empty packets
    if (packet.size() <= sizeof(QosFrame) + sizeof(uint16_t))
    {
        return;
    }

    // Skip 2 bytes of padding
    const Bytes data(packet, sizeof(QosFrame) + sizeof(uint16_t));

    std::lock_guard<std::mutex> lock(controllerMutex);

    // Ignore unconnected controllers
    if (!controllers[wcid - 1])
    {
#ifdef _DEBUG
        // The one drop above GipDevice that could hide a second client. A GIP accessory is
        // supposed to share its pad's wireless client and be told apart by device id, not to
        // associate separately - but that is an assumption, and this is where a packet would
        // vanish if it were wrong. Reported so that "no accessory ever appeared" can be stated
        // about the whole path rather than only about the parser.
        Log::info("Data frame for wcid %u with no controller", (unsigned)wcid);
#endif

        return;
    }

    if (!controllers[wcid - 1]->handlePacket(data))
    {
        Log::error("Error handling packet for controller '%d'", wcid);
    }
}

void Dongle::handleWlanPacket(const Bytes &packet)
{
    // Ignore invalid or empty packets
    if (packet.size() <= sizeof(RxWi) + sizeof(WlanFrame))
    {
        Log::debug("WlanPacket size error");
        return;
    }

    const RxWi *rxWi = packet.toStruct<RxWi>();
    const WlanFrame *wlanFrame = packet.toStruct<WlanFrame>(sizeof(RxWi));

    const Bytes source(
        wlanFrame->source,
        wlanFrame->source + macAddress.size()
    );
    const Bytes destination(
        wlanFrame->destination,
        wlanFrame->destination + macAddress.size()
    );

    // Packet has wrong destination address
    if (destination != macAddress)
    {
        return;
    }

    uint8_t type = wlanFrame->frameControl.type;
    uint8_t subtype = wlanFrame->frameControl.subtype;

    if (type == MT_WLAN_MANAGEMENT)
    {
        Log::debug("handle MGMT frame");
        switch (subtype)
        {
            case MT_WLAN_ASSOCIATION_REQ:
                handleControllerConnect(source);
                break;

            // Only kept for compatibility with 1537 controllers
            // They associate, disassociate and associate again during pairing
            // Disassociations happen without triggering EVT_CLIENT_LOST
            case MT_WLAN_DISASSOCIATION:
                handleControllerDisconnect(rxWi->wcid);
                break;

            // Reserved frames are used for different purposes
            // Most of them are yet to be discovered
            case MT_WLAN_RESERVED:
                const Bytes innerPacket(
                    packet,
                    sizeof(RxWi) + sizeof(WlanFrame)
                );

                handleControllerPair(source, innerPacket);
                break;
        }
    }

    else if (type == MT_WLAN_DATA && subtype == MT_WLAN_QOS_DATA)
    {
        const Bytes innerPacket(
            packet,
            sizeof(RxWi) + sizeof(WlanFrame)
        );

        handleControllerPacket(rxWi->wcid, innerPacket);
    }
}

void Dongle::handleBulkData(const Bytes &data)
{
    // Ignore invalid or empty data
    if (data.size() <= sizeof(RxInfoGeneric) + sizeof(uint32_t))
    {
        return;
    }

    // Skip packet end marker (4 bytes, identical to header)
    const RxInfoGeneric *rxInfo = data.toStruct<RxInfoGeneric>();
    const Bytes packet(data, sizeof(RxInfoGeneric), sizeof(uint32_t));

    if (rxInfo->port == CPU_RX_PORT)
    {
        const RxInfoCommand *info = data.toStruct<RxInfoCommand>();
        switch (info->eventType)
        {
            case EVT_BUTTON_PRESS:
                // Doesn't need controllerMutex, but does need pairingMutex, which
                // setPairing() takes: the app can request a pairing change concurrently.
                setPairing(true);
                break;

            case EVT_PACKET_RX:
                handleWlanPacket(packet);
                break;

            case EVT_CLIENT_LOST:
                // Packet is guaranteed not to be empty
                handleControllerDisconnect(packet[0]);
                break;
        }
    }

    else if (rxInfo->port == WLAN_PORT)
    {
        const RxInfoPacket *info = data.toStruct<RxInfoPacket>();

        if (info->is80211)
        {
            handleWlanPacket(packet);
        }
    }
}

void Dongle::readBulkPackets(uint8_t endpoint)
{
    FixedBytes<USB_MAX_BULK_TRANSFER_SIZE> buffer;

    // Sunburst: the driver makes no Java calls, so the read thread does not
    // attach to a JVM.
    while (!stopThreads)
    {
        int transferred = usbDevice->bulkRead(endpoint, buffer);

        // Bulk read failed
        if (transferred < 0)
        {
            break;
        }

        if (transferred > 0)
        {
            Bytes data = buffer.toBytes(transferred);

            handleBulkData(data);
        }
    }
}

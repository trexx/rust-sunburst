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

#pragma once

#include "mt76.h"
#include "../controller/controller.h"

#include <cstdint>
#include <array>
#include <atomic>
#include <thread>
#include <mutex>

// Microsoft's vendor ID
#define DONGLE_VID 0x045e

// Product IDs for both versions of the dongle
#define DONGLE_PID_OLD 0x02e6
#define DONGLE_PID_NEW 0x02fe

// Product ID for Microsoft Surface Book 2 built-in dongle
#define DONGLE_PID_SURFACE 0x091e

/*
 * Handles received 802.11 packets
 * Delegates GIP (Game Input Protocol) packets to controllers
 */
class Dongle : public Mt76
{
public:
    /*
     * @param ledBrightness guide button LED intensity handed to every controller this adapter
     *                      brings up, as the protocol's own field. Held rather than applied here:
     *                      the adapter has no LED of its own, and pads appear asynchronously.
     */
    Dongle(std::unique_ptr<UsbDevice> usbDevice, GipSink *sink, uint8_t ledBrightness = 0x14);
    ~Dongle();

    bool start(std::string);
    void stop();

    /**
     * Turns pairing mode on or off, serialised against the read threads.
     *
     * <p>Local addition. Upstream only ever reaches pairing mode from the adapter's physical
     * button, which is dead on some units; this is the same state Windows sets from
     * "Add a device", exposed so the app can offer it. Every caller goes through here rather
     * than Mt76::setPairingStatus so the beacon write and the LED command cannot interleave
     * between the two read threads and the app.
     */
    bool setPairing(bool enable);

private:
    /* Packet handling */
    void handleControllerConnect(Bytes address);
    void handleControllerDisconnect(uint8_t wcid);
    void handleControllerPair(Bytes address, const Bytes &packet);
    void handleControllerPacket(uint8_t wcid, const Bytes &packet);
    void handleWlanPacket(const Bytes &packet);
    void handleBulkData(const Bytes &data);
    void readBulkPackets(uint8_t endpoint);

    // Sunburst: report add/remove to the C++ sink (the shim) instead of a Java
    // object. The wireless address that upstream forwarded for pad-number
    // stability is not needed — the shim keys its 0..3 pad index on the slot.
    void notifyControllerAdd(int id, Controller *controller, short vid, short pid);
    void notifyControllerRemove(int id);

    GipSink *sink;
    std::vector<std::thread> threads;
    std::atomic<bool> stopThreads;

    // Passed to each Controller as it is constructed; see the constructor's comment
    const uint8_t ledBrightness;

    std::mutex controllerMutex;
    // Guards setPairing() only. Both read threads can reach pairing changes - one via the
    // button event, the other via a controller completing pairing - and the app can too.
    std::mutex pairingMutex;
    std::array<std::unique_ptr<Controller>, MT_WCID_COUNT> controllers;
    // MAC address of the controller occupying each slot, parallel to 'controllers' above and
    // guarded by the same mutex. Only meaningful where 'controllers' holds a live entry; it is
    // what lets handleControllerConnect() recognise a retransmitted association request.
    std::array<Bytes, MT_WCID_COUNT> clientAddresses;
};

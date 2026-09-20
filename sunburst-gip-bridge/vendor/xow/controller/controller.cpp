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

#include "controller.h"
#include "../utils/log.h"
#include "../utils/crypto.h"
#include "gip.h"

#include <cstdlib>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <algorithm>
#include <string>
#include <utility>
#include <linux/input.h>

// Configuration for the compatibility mode
#define COMPATIBILITY_ENV "XOW_COMPATIBILITY"
#define COMPATIBILITY_NAME "Microsoft X-Box 360 pad"
#define COMPATIBILITY_PID 0x028e
#define COMPATIBILITY_VERSION 0x0104

// Accessories use IDs greater than zero
#define DEVICE_ID_CONTROLLER 0
#define DEVICE_NAME "Xbox One Wireless Controller"

#define INPUT_STICK_FUZZ 255
#define INPUT_STICK_FLAT 4095
#define INPUT_TRIGGER_FUZZ 3
#define INPUT_TRIGGER_FLAT 63

// Motor levels are a percentage, not a raw byte: MS-GIPUSB v20240916 section 3.1.5.6.1
// (Direct Motor Command) specifies every level field as "Percentage, 0 - 100% (0x00 to 0x64),
// of PWM for motor". Anything above 0x64 is out of spec.
// Metadata element item widths. These are the sizes of the structures each element is an array
// of, per MS-GIPUSB 2.2.2.4 and xone's reading of it: a command descriptor is 23 bytes, a firmware
// version is two 16-bit halves, an audio format is a 2-byte pair and an interface is a GUID.
#define METADATA_COMMAND_LENGTH 23
#define METADATA_VERSION_LENGTH 4
#define METADATA_AUDIO_FORMAT_LENGTH 2
#define METADATA_GUID_LENGTH 16

#define RUMBLE_MAX_POWER 100
#define RUMBLE_DELAY std::chrono::milliseconds(10)

// Power level from the status byte's top two bits (MS-GIPUSB Table 30). Only "powering off or
// resetting" is acted on; 01 is unused, 10 is full power and 11 is reserved.
#define POWER_LEVEL_OFF 0x00

// Bytes per second of 48 kHz 16-bit stereo, which is what the ring holds whatever the transport
// packetises it into: 48000 Hz * 2 channels * 2 bytes.
#define AUDIO_BYTES_PER_SECOND (48000 * 2 * 2)

// Four packets, so roughly four cadences may queue before samples start being dropped. Enough to
// ride out a scheduling hiccup on either thread, short enough that a real backlog is discarded
// rather than turning into permanent lag.
#define AUDIO_BUFFER_MAX_BYTES (audioPacketBytes * 4)

// How far the pad's requested flow rate may sit from our packet size before it is worth saying so.
//
// MS-GIPUSB 3.2.5.1.5 has the device modulate this by "+/- one sample per channel per 1 ms" to
// absorb clock drift, which over an 8 ms packet is 8 samples per channel: 8 * 2 channels * 2 bytes.
// Anything inside that band is the protocol working, not a fault, and logging it would be noise.
#define AUDIO_FLOW_RATE_TOLERANCE (8 * 2 * 2)

/*
 * 2.2.11's volume wait, in the units it states it in: "the host sends Set Device State: START at
 * 500 ms intervals until receipt of the volume message, or until it times out after 3 seconds".
 */
#define AUDIO_VOLUME_INTERVAL_MS 500
#define AUDIO_VOLUME_ATTEMPTS 6

/*
 * One cadence plus a whole audio frame. Past this the host has failed to supply, which is what a
 * gap sounds like; short of it the sender is simply waiting, which is how it paces itself.
 *
 * The allowance is a frame rather than a fraction of the cadence because that is what actually
 * bounds the wait. The host delivers whole Opus frames - 5 ms, or 10 ms for a slow decoder or a
 * low bitrate - and their size does not divide our packet size, so the sender routinely waits for
 * the next frame to complete one packet. Scaling the threshold off the cadence instead left the
 * margin dependent on the transport: 2 ms on the adapter's 8 ms packet, which read a useful 1.4%,
 * but 1 ms on a cabled pad's 4 ms packet, which read 31% through audio that was perfect.
 *
 * That is the second time this counter has been recalibrated for alarming on healthy behaviour,
 * and the lesson is the same both times: it is a fault detector, not a jitter meter.
 */
#define AUDIO_STARVE_TIMEOUT std::chrono::microseconds( \
    (audioPacketBytes * 1000000LL) / AUDIO_BYTES_PER_SECOND + 10000)

// Scales a 16-bit magnitude, which is what moonlight-common-c and the Android input APIs both
// deal in, onto the protocol's 0 - 100 range.
#define RUMBLE_SCALE(magnitude) \
    static_cast<uint8_t>((static_cast<uint32_t>(magnitude) * RUMBLE_MAX_POWER) / UINT16_MAX)

// Initialiser order follows declaration order in the header, which -Wreorder checks
Controller::Controller(
    SendPacket sendPacket,
    uint8_t ledBrightness
) : GipDevice(std::move(sendPacket)),
    stopRumbleThread(false), ledBrightness(ledBrightness) {}

Controller::~Controller()
{
    {
        std::lock_guard<std::mutex> lock(startMutex);

        stopStartThread = true;
    }

    startCondition.notify_one();

    if (startThread.joinable())
    {
        startThread.join();
    }

    stopRumbleThread = true;
    rumbleCondition.notify_one();

    if (rumbleThread.joinable())
    {
        rumbleThread.join();
    }

    {
        std::lock_guard<std::mutex> lock(volumeMutex);

        stopVolumeThread = true;
    }

    volumeCondition.notify_one();

    if (volumeThread.joinable())
    {
        volumeThread.join();
    }

    // Stops and joins the audio thread if it is running. Must happen before the JNI teardown
    // below, since the thread touches nothing Java but does use this object's sendPacket.
    setAudioEnabled(false);

    /*
     * The stream is left running between sessions on purpose - the device is configured once and
     * carries silence when there is nothing to play - so going away is the one moment it genuinely
     * has to be stopped. Nothing else will: 2.2.11 has it stream until told otherwise, and a pad
     * on a cable is not re-enumerated when we exit.
     */
    if (audioDeviceId != 0)
    {
        if (audioTransport != nullptr)
        {
            audioTransport->disableAudio();
        }

        if (!setDeviceState(audioDeviceId, STATE_STOP))
        {
            Log::error("Failed to stop the audio device on teardown");
        }
    }

    /*
     * Only where the device is ours to switch off. A pad on the adapter is: when this driver stops,
     * nothing else is driving it, and leaving it powered would drain a battery.
     *
     * A cabled pad is not. Android's own driver takes it back the moment we let go, and telling it
     * to power off leaves it unresponsive to whoever picks it up next - including us on the next
     * connect, which showed up as a pad that would not answer a metadata request until it had been
     * physically unplugged and replugged.
     */
    if (powerOffOnTeardown && !setDeviceState(DEVICE_ID_CONTROLLER, STATE_OFF))
    {
        Log::error("Failed to turn off controller");
    }
    // Sunburst: no Java references to release — input/battery/audio-removal go
    // through the C++ `GipSink`, set by the owning Dongle/WiredController.
}

void Controller::setSink(GipSink *s, int slotIndex) {
    sink = s;
    slot = slotIndex;
}

void Controller::deviceAnnounced(uint8_t id, const AnnounceData *announce)
{
    Log::info("Device %u announced, vendor %04x product %04x",
              id, announce->vendorId, announce->productId);

    /*
     * A sub-device, which for this pad means the 3.5 mm audio one: MS-GIPUSB 2.2.1.4 has these
     * enumerate only once the primary has completed the security handshake, and Table 1 shows them
     * carrying their own device ID, VID and PID.
     *
     * It must not go through initInput(). That is the primary device's setup - it ends by assigning
     * to rumbleThread, and assigning over a thread that is already running calls std::terminate.
     * A sub-device has no rumble, no input reports and no LED; what it has is its own metadata,
     * which is where its audio formats are stated.
     */
    if (id != DEVICE_ID_CONTROLLER)
    {
        // Recorded before the metadata is asked for, because the metadata handler is what acts on
        // it: an announce means this sub-device has been through Arrival and is ours to configure.
        audioDeviceAnnounced = true;

        if (!requestIdentify(id))
        {
            Log::error("Failed to request metadata from device %u", id);
        }

        return;
    }

    Log::debug(
        "Firmware version: %d.%d.%d.%d",
        announce->firmwareVersion.major,
        announce->firmwareVersion.minor,
        announce->firmwareVersion.build,
        announce->firmwareVersion.revision
    );
    Log::debug(
        "Hardware version: %d.%d",
        announce->hardwareVersion.major,
        announce->hardwareVersion.minor
    );

    // Logged because the security version decides which handshake a pad expects, and because the
    // specification requires all three to be 1.0 - anything else is a device worth knowing about.
    Log::debug(
        "Protocol versions: RF %d.%d, security %d.%d, GIP %d.%d",
        announce->rfVersion.major,
        announce->rfVersion.minor,
        announce->securityVersion.major,
        announce->securityVersion.minor,
        announce->gipVersion.major,
        announce->gipVersion.minor
    );

    /*
     * A device in Arrival re-sends Hello every 500 ms until the host answers (MS-GIPUSB 2.2.1), so
     * a second one is routine rather than an error - and on a cable nothing above this dedupes
     * them, where the dongle drops duplicate associations before they reach here.
     *
     * Answering again is right; rebuilding the input state is not. initInput() assigns to
     * rumbleThread, and assigning over a thread that is already running calls std::terminate.
     */
    if (inputInitialised.exchange(true))
    {
        Log::debug("Device re-announced, answering without reinitialising");

        if (!requestIdentify())
        {
            Log::error("Failed to answer a repeated announce");
        }

        return;
    }

    initInput();
}

void Controller::statusReceived(uint8_t id, const StatusData *status)
{
    const std::string levels[] = { "critically low", "low", "medium", "full" };
    const std::string charges[] = { "not charging", "charging", "charge error", "reserved" };

    uint8_t type = status->batteryType;
    uint8_t rawPower = (status->connectionInfo >> 2) & 0x03;

    /*
     * A sub-device's status is about the sub-device, not the pad.
     *
     * The id was accepted and then ignored here, so a 3.5 mm audio device reporting its own
     * removal was read as the *controller's* battery and power state. Unplugging a headset
     * therefore logged "Battery: absent, running on external power" and "Controller is powering
     * off or resetting" about a pad that was doing neither - and worse, pushed both to the host,
     * which was told the pad had no battery and was shutting down. It self-corrected on the pad's
     * next real status, but only because the values then differed enough to pass the change filter
     * below.
     *
     * MS-GIPUSB Table 30 makes the removal unambiguous: bits 7:6 are Power level and 00 is
     * "Device powering off/resetting". The whole status byte reads zero here, which 3.1.5.5.2 says
     * "is not valid except for a USB-only device without batteries that is powering off" - an
     * exact description of an audio sub-device on a headphone jack.
     *
     * Note this is not xone's reading, which takes bit 7 alone as a connected flag. That gets the
     * common cases right because Full Power is 10, but the field is two bits wide.
     */
    if (id != DEVICE_ID_CONTROLLER)
    {
        // exchange() rather than a load-then-store, so two reports arriving together cannot both
        // get through and start a teardown each
        if (rawPower == POWER_LEVEL_OFF && id == audioDeviceId &&
            !audioRemovalNotified.exchange(true))
        {
            Log::info("Audio device %u reports powering off; forgetting it", (unsigned)id);

            notifyAudioDeviceRemoved();
        }

        return;
    }

    uint8_t level = status->batteryLevel;

    // MS-GIPUSB Table 30 packs four fields into this byte: battery level in 1:0, battery type in
    // 3:2, charge state in 5:4 and power level in 7:6. xow's struct names only the low two and
    // lumps the top nibble into connectionInfo, which is why the charge state looked absent - the
    // protocol does report it, this driver was simply throwing it away.
    uint8_t charge = status->connectionInfo & 0x03;
    uint8_t power = rawPower;

    // Nothing has moved since the last report
    if (type == batteryType && level == batteryLevel &&
        charge == batteryCharge && power == powerLevel)
    {
        return;
    }

    batteryType = type;
    batteryLevel = level;
    batteryCharge = charge;
    powerLevel = power;

    // Type 0 is "battery absent", not "charging" - BATT_TYPE_CHARGING is a misnomer inherited
    // from xow, whose name is left alone so gip.h stays refreshable (see UPSTREAM.md). Table 30
    // is explicit: 00 absent or bus powered, 01 standard/alkaline, 10 rechargeable. A pad with no
    // battery has no meaningful level, which is why this case used to return early - and why
    // battery never reached the host at all, since the state most worth reporting was dropped.
    if (type == BATT_TYPE_CHARGING)
    {
        Log::info("Battery: absent, running on external power");
    }
    else
    {
        Log::info("Battery: %s, %s", levels[level].c_str(), charges[charge].c_str());
    }

    // 00 means the pad is powering off or resetting, which is worth seeing in a log next to a
    // disconnect that would otherwise look unexplained.
    if (power == POWER_LEVEL_OFF)
    {
        Log::info("Controller is powering off or resetting");
    }

    notifyJavaBattery(type, level, charge);
}

/*
 * Whether a message addressed to 'id' is this pad's own.
 *
 * This class models one physical controller, so input, the guide button and the serial number only
 * mean anything from device 0. A sub-device sending them is not a malformed packet - a chatpad is a
 * sub-device that sends input reports, and xone drives one - it is simply something this driver
 * does not model, and taking it as the pad's own would report a chatpad's keys as stick movement.
 *
 * Reported in debug builds rather than dropped in silence, for the same reason
 * logAccessoryPacket() exists: "no sub-device ever sent input" is only worth anything if it can be
 * distinguished from "nobody looked".
 */
bool Controller::isPrimary(uint8_t id, const char *what)
{
    if (id == DEVICE_ID_CONTROLLER)
    {
        return true;
    }

    Log::debug("Ignoring %s from sub-device %u", what, (unsigned)id);

    return false;
}

void Controller::serialNumberReceived(uint8_t id, const SerialData *serial)
{
    if (!isPrimary(id, "serial number"))
    {
        return;
    }

    const std::string number(
        serial->serialNumber,
        sizeof(serial->serialNumber)
    );

    Log::info("Serial number: %s", number.c_str());
}

#define SET_BUTTON_STATUS(flag, ok) do{ \
    if(ok) {                            \
        buttonStatus |= flag;           \
    } else {                            \
        buttonStatus &= ~flag;          \
    }                                   \
} while(0);

void Controller::guideButtonPressed(uint8_t id, const GuideButtonData *button)
{
    if (!isPrimary(id, "guide button"))
    {
        return;
    }

    SET_BUTTON_STATUS(SPECIAL_BUTTON_FLAG, button->pressed);

    // Already established as the pad's, so this reports as the pad rather than re-checking
    inputReceived(DEVICE_ID_CONTROLLER, nullptr);
}

void Controller::updateButtonStatus(const GipDevice::InputData *input) {
    SET_BUTTON_STATUS(PLAY_FLAG, input->buttons.start);
    SET_BUTTON_STATUS(BACK_FLAG, input->buttons.select);
    SET_BUTTON_STATUS(A_FLAG, input->buttons.a);
    SET_BUTTON_STATUS(B_FLAG, input->buttons.b);
    SET_BUTTON_STATUS(X_FLAG, input->buttons.x);
    SET_BUTTON_STATUS(Y_FLAG, input->buttons.y);
    SET_BUTTON_STATUS(UP_FLAG, input->buttons.dpadUp);
    SET_BUTTON_STATUS(DOWN_FLAG, input->buttons.dpadDown);
    SET_BUTTON_STATUS(LEFT_FLAG, input->buttons.dpadLeft);
    SET_BUTTON_STATUS(RIGHT_FLAG, input->buttons.dpadRight);
    SET_BUTTON_STATUS(LB_FLAG, input->buttons.bumperLeft);
    SET_BUTTON_STATUS(RB_FLAG, input->buttons.bumperRight);
    SET_BUTTON_STATUS(LS_CLK_FLAG, input->buttons.stickLeft);
    SET_BUTTON_STATUS(RS_CLK_FLAG, input->buttons.stickRight);
    this->triggerLeft = input->triggerLeft;
    this->triggerRight = input->triggerRight;
    this->stickLeftX = input->stickLeftX;
    this->stickLeftY = input->stickLeftY;
    this->stickRightX = input->stickRightX;
    this->stickRightY = input->stickRightY;
}
#undef SET_BUTTON_STATUS

void Controller::inputReceived(uint8_t id, const InputData *input)
{
    if (!isPrimary(id, "input")) {
        return;
    }

    if(input) {
        updateButtonStatus(input);
    }

    if (sink) {
        sink->onInput(slot, buttonStatus, triggerLeft, triggerRight, stickLeftX, stickLeftY,
                      stickRightX, stickRightY);
    }
}

void Controller::notifyJavaBattery(uint8_t type, uint8_t level, uint8_t charge)
{
    // Raw GIP values; the shim maps them onto Sunburst's `Battery` fields.
    if (sink) {
        sink->onBattery(slot, type, level, charge);
    }
}

void Controller::initInput()
{
    // Ask for metadata and wait for the answer before starting the device. MS-GIPUSB 3.1.1 has the
    // device go Arrival -> Idle on the metadata request and Idle -> Active on Set Device State, and
    // 2.2.11 has an audio sub-device send its own Hello 500-1000 ms after "the primary device
    // initializes". Starting the device while it is still transmitting metadata gets that order
    // wrong, which is the one part of this handshake we can see is not what the spec describes.
    //
    // What comes back is also worth having on its own: the audio formats a pad declares are the
    // only statement of what it can be sent, and this is where they arrive.
    if (!requestIdentify())
    {
        Log::error("Failed to request metadata");

        // Nothing will answer, so do not wait for it
        startDevice();
    }
    else
    {
        startThread = std::thread(&Controller::waitForMetadata, this);
    }

    rumbleThread = std::thread(&Controller::processRumble, this);
}

/*
 * Logs what the controller reports it can do. Each offset points at a count byte followed by that
 * many fixed-size items, so an element is only read once its count has been bounds-checked against
 * the payload actually received.
 */
/*
 * Locates one metadata element. Each offset points at a count byte followed by that many
 * fixed-size items; a zero offset means the device has none. Returns the item block, or nullptr
 * if the element is absent or would run past what actually arrived.
 */
static const uint8_t *findInfoElement(const uint8_t *payload, size_t length, uint16_t offset,
                                      size_t itemLength, uint8_t &count)
{
    count = 0;

    if (offset == 0 || offset >= length)
    {
        return nullptr;
    }

    count = payload[offset];

    if (count == 0 || offset + 1 + static_cast<size_t>(count) * itemLength > length)
    {
        count = 0;

        return nullptr;
    }

    return payload + offset + 1;
}

/*
 * Logs an element as raw bytes, for the ones whose contents this driver does not interpret.
 */
static void logInfoElement(const char *name, const uint8_t *payload, size_t length,
                           uint16_t offset, size_t itemLength)
{
    uint8_t count;
    const uint8_t *items = findInfoElement(payload, length, offset, itemLength, count);

    if (items == nullptr)
    {
        return;
    }

    std::string hex;

    for (size_t i = 0; i < static_cast<size_t>(count) * itemLength; i++)
    {
        char byte[4];

        snprintf(byte, sizeof(byte), "%02x ", items[i]);
        hex += byte;
    }

    Log::info("Metadata %s: %u item(s): %s", name, count, hex.c_str());
}

/*
 * Logs the client command descriptors, which say which data-class messages the device actually
 * speaks. Worth decoding rather than dumping: this is the element that answers "can this pad do
 * audio at all", and a pad that lists only 0x20 and 0x09 has input and rumble and nothing else.
 */
static void logCommandDescriptors(const uint8_t *payload, size_t length, uint16_t offset)
{
    uint8_t count;
    const uint8_t *items = findInfoElement(payload, length, offset,
                                           METADATA_COMMAND_LENGTH, count);

    if (items == nullptr)
    {
        return;
    }

    std::string list;

    for (uint8_t i = 0; i < count; i++)
    {
        const uint8_t *item = items + static_cast<size_t>(i) * METADATA_COMMAND_LENGTH;
        char entry[32];

        // Layout per xone's gip_command_descriptor: marker, unknown, command, length
        snprintf(entry, sizeof(entry), "%02x(len %u) ", item[2], item[3]);
        list += entry;
    }

    Log::info("Metadata commands: %u item(s): %s", count, list.c_str());
}

void Controller::authCompleted(const uint8_t *sessionKey, size_t length)
{
    // The handshake is what gates sub-device enumeration (2.2.1.4), so this is the first moment
    // asking could succeed. The session key is for link encryption, which this driver does not do.
    probeSubDevices();
}

void Controller::probeSubDevices()
{
    /*
     * The expansion index is three bits (2.2.10), so 1 to 7 is every sub-device a primary can
     * have; zero is the primary itself. Seven small requests once per connect is nothing next to
     * being unable to find the audio device at all.
     *
     * The observed audio sub-device answers on 3 rather than the 1 that Table 1's example uses,
     * which is why this asks all of them rather than the one the specification happens to
     * illustrate.
     */
    for (uint8_t id = 1; id <= 7; id++)
    {
        if (!requestIdentify(id))
        {
            Log::error("Failed to ask device %u for metadata", id);

            return;
        }
    }
}

void Controller::identifyReceived(uint8_t id, const IdentifyData *identify,
                                  const uint8_t *payload, size_t length)
{
    Log::info("Metadata from device %u: %zu bytes", id, length);

    // Item widths are xone's, which are the sizes of the structs it maps each element onto:
    // gip_command_descriptor is 23 bytes and gip_firmware_version is 4. The 1 and 8 used here
    // before were guesses, and the firmware element overran into the ones logged after it.
    logCommandDescriptors(payload, length, identify->clientCommandsOffset);
    logInfoElement("firmware versions", payload, length,
                   identify->firmwareVersionsOffset, METADATA_VERSION_LENGTH);
    logInfoElement("audio formats", payload, length,
                   identify->audioFormatsOffset, METADATA_AUDIO_FORMAT_LENGTH);
    logInfoElement("capabilities out", payload, length, identify->capabilitiesOutOffset, 1);
    logInfoElement("capabilities in", payload, length, identify->capabilitiesInOffset, 1);
    logInfoElement("interfaces", payload, length, identify->interfacesOffset, METADATA_GUID_LENGTH);

    // Retained rather than only logged: whether a device has a usable audio format is the one
    // thing a caller needs before offering to send it audio, and metadata is the only place that
    // says so. An empty list means the device declared none.
    uint8_t formats;
    const uint8_t *items = findInfoElement(payload, length, identify->audioFormatsOffset,
                                           METADATA_AUDIO_FORMAT_LENGTH, formats);

    std::vector<uint8_t> parsed;

    if (items != nullptr)
    {
        parsed.assign(items,
                      items + static_cast<size_t>(formats) * METADATA_AUDIO_FORMAT_LENGTH);
    }

    if (id == DEVICE_ID_CONTROLLER)
    {
        audioFormats = parsed;
    }
    else if (!parsed.empty())
    {
        // The audio sub-device, which is where audio actually lives. Its formats are pairs of
        // capture and render, and 2.2.11 has the host take the first rather than choose.
        Log::info("Audio device %u offers %zu format pair(s), first %02x/%02x, announced %s",
                  id, parsed.size() / METADATA_AUDIO_FORMAT_LENGTH, parsed[0], parsed[1],
                  audioDeviceAnnounced.load() ? "yes" : "no");

        /*
         * Configured wherever it is, including when it never announced.
         *
         * 2.2.1 has a device send Hello only while in Arrival, so a sub-device left Active by a
         * killed process never announces - and on hardware that is exactly the sessions that
         * stutter: five in a row, every one with an announce clean and every one without it not,
         * while our own side reported 100% supply and no underruns in both. The announce state is
         * logged above so this stays visible rather than inferred.
         *
         * **Resetting it does not bring it back.** 3.1.1 says a device reinitialises on RESET and
         * then "SHOULD send GIP Hello's at 500 ms intervals until the host responds", so a reset
         * here should produce the announce we are missing. It does not: this pad answers a
         * sub-device RESET with silence and never announces again, which leaves it with no audio
         * device at all until the cable is pulled. That was tried, on hardware, and reverted - it
         * turned stuttering audio into "no headset detected", which is worse.
         *
         * So there is no known way back to Arrival for a sub-device short of unplugging, and this
         * configures what is there. See AUDIO.md for what else has been tried.
         */
        audioDeviceId = id;
        audioDeviceFormats = parsed;

        // A jack re-inserted after a removal adopts a device again, so the latch must not still be
        // set from the last one or its removal would go unreported
        audioRemovalNotified.store(false);

        /*
         * Silenced on discovery, because we may not be the first host to have configured it.
         *
         * 2.2.11 keeps a started audio device streaming "until the device is powered off,
         * disconnected, or until the host requests a new audio configuration first through
         * transmission of a Set Device State: STOP" - so a process that exits without sending one
         * leaves the device playing, and on a cable nothing re-enumerates it in between. It was
         * heard as repeating noise the moment a stream started, before audio had been enabled at
         * all, because the previous run's stream had never stopped.
         */
        if (!setDeviceState(audioDeviceId, STATE_STOP))
        {
            Log::error("Failed to silence the audio device on discovery");
        }

        /*
         * Nothing else is sent here, and in particular the device is not configured.
         *
         * A previous version proposed the device's *second* advertised pair at this point, so that
         * the real one would register as a change rather than a repeat. It was a hypothesis against
         * the leftover-stream fault, it was recorded as having no effect on that fault, and it was
         * left in place - which made it a per-startup renegotiation, the one thing 2.2.11 and xone
         * agree must not happen. "Doing it per session degraded the pad a step each time" is this
         * file's own hardest-won rule; this was doing it per startup.
         *
         * On the pad here the two pairs are 09/10 and 09/09, and 3.2.5.1.2 gives 0x09 as 24 kHz
         * mono against 0x10's 48 kHz stereo. So it was retuning the render path to 24 kHz mono and
         * back on every connect, seconds apart, and 2.2.11 has the device reconfigure its audio
         * hardware before it answers. A render pipeline left anywhere between those two is heard
         * exactly as the reported fault: gapped, and pitched down.
         *
         * The format is proposed once, in setAudioEnabled(), and never again.
         */
    }

    // The metadata exchange is done, so the device is in Idle and can be started. Wakes the
    // fallback waiter too, so it stops waiting rather than sitting out its full timeout.
    startDevice();
    startCondition.notify_one();
}

void Controller::startDevice()
{
    if (deviceStarted.exchange(true))
    {
        return;
    }

    if (!setDeviceState(DEVICE_ID_CONTROLLER, STATE_START))
    {
        Log::error("Failed to start controller");

        return;
    }

    LedModeData ledMode = {};

    /*
     * Guide button brightness, from the user's setting.
     *
     * The range is 0x00 to 0x2F, not the 0x00 to 0x20 xow claimed here: MS-GIPUSB 3.1.5.5.7
     * Table 41 gives the intensity field as "0 - 47%", and xone caps it at 50. Whether a pad
     * actually honours anything above 0x20 is untested - see HARDWARE_TESTING.md 22.
     *
     * Pattern and intensity are separate fields (Table 42), so an intensity of zero on a lit
     * pattern is not what the spec means by off. A zero brightness therefore selects the off
     * pattern rather than a maximally dimmed on one.
     */
    ledMode.mode = ledBrightness == 0 ? LED_OFF : LED_ON;
    ledMode.brightness = ledBrightness;

    if (!setLedMode(ledMode))
    {
        Log::error("Failed to set initial LED mode");

        return;
    }

    Log::info("Guide button LED set to mode %d intensity 0x%02x", ledMode.mode, ledBrightness);

    if (!requestSerialNumber())
    {
        Log::error("Failed to request serial number");
    }

    // The security exchange, once the device is Active. A capture of a Windows host has it here -
    // set state, LED, then the handshake, about 170 ms after the announce - and sending it any
    // earlier, while the pad is still in Arrival, got no answer at all. Its audio sub-device
    // appears seconds after this completes; this driver has never sent it and none has appeared.
    // Failing is not fatal, the driver has always run without it.
    if (!sendAuthHostHello())
    {
        Log::error("Failed to start the security exchange");
    }
}

void Controller::waitForMetadata()
{
    // Sunburst: the security handshake's crypto is now Rust (sb_crypto_*), not
    // Java, so this thread no longer needs a JNIEnv and does not attach.
    std::unique_lock<std::mutex> lock(startMutex);

    // The same 500 ms the spec gives a device for assuming a lost state message (MS-GIPUSB 3.1.1).
    // Metadata has arrived within about 60 ms on every pad measured here, so this is a fallback
    // for one that never answers rather than a delay anything normally waits out.
    startCondition.wait_for(lock, std::chrono::milliseconds(500),
                            [this] { return stopStartThread || deviceStarted.load(); });

    if (stopStartThread)
    {
        return;
    }

    if (!deviceStarted.load())
    {
        Log::info("No metadata after 500 ms; starting the controller anyway");
    }

    startDevice();
}

void Controller::audioSamplesReceived(uint8_t id, const AudioSamplesData *samples,
                                      const uint8_t *pcm, size_t pcmLen)
{
    /*
     * The audio sub-device's, not the pad's, and not another sub-device's. The flow rate carried
     * here paces a transport that can honour it, so taking it from the wrong device would be
     * pacing the audio stream by something unrelated to it.
     */
    if (id != audioDeviceId)
    {
        return;
    }

    // Retain the microphone PCM (if any) before the flow-rate early-out below: capture samples
    // arrive on every message, but the flow rate usually holds steady, so gating retention on a
    // rate change would drop almost all of the mic audio.
    if (pcmLen > 0)
    {
        captureAudioReceived(pcm, pcmLen);
    }

    // How many bytes of render data the pad wants in each message (MS-GIPUSB Table 69). It nudges
    // this up and down to absorb the difference between its clock and ours, and per 3.2.5.1.5 that
    // is "the mechanism GIP devices use to eliminate pops and clicks in audio".
    //
    // A transport that can hear these honours it through audioRenderBytes(); the adapter cannot,
    // because its audio is one 8 ms message rather than eight 1 ms ones, so there it is still only
    // a health signal. Small movement is expected and says nothing; only a request well outside
    // the band the spec describes is worth reporting.
    if (samples->flowRate == audioFlowRate.load(std::memory_order_relaxed))
    {
        return;
    }

    audioFlowRate.store(samples->flowRate, std::memory_order_relaxed);

    /*
     * Compared against what a message actually carries on this transport, which is not the same
     * quantity on both. The adapter sends one message per 8 ms buffer, so the device asks in whole
     * buffers; a cabled pad sends one per millisecond, so it asks in milliseconds. Comparing a
     * per-millisecond request against the buffer size reported every healthy rate as an excursion.
     */
    size_t expected = audioTransport != nullptr ? AUDIO_BYTES_PER_SECOND / 1000
                                                : audioPacketBytes;

    int delta = static_cast<int>(audioFlowRate.load(std::memory_order_relaxed))
              - static_cast<int>(expected);

    if (delta > AUDIO_FLOW_RATE_TOLERANCE || delta < -AUDIO_FLOW_RATE_TOLERANCE)
    {
        Log::debug("Audio flow rate %u is outside the expected band around %u",
                   audioFlowRate.load(std::memory_order_relaxed), (unsigned)expected);
    }
}

void Controller::captureAudioReceived(const uint8_t *pcm, size_t bytes)
{
    if (pcm == nullptr || bytes == 0)
    {
        return;
    }

    std::lock_guard<std::mutex> lock(captureMutex);

    // Drop the newest bytes rather than grow without bound, the same choice queueAudio() makes on
    // the render ring: a mic that nothing is draining must not accumulate a session's worth.
    if (captureBuffer.size() >= CAPTURE_BUFFER_MAX)
    {
        return;
    }

    size_t room = CAPTURE_BUFFER_MAX - captureBuffer.size();
    size_t take = bytes < room ? bytes : room;
    captureBuffer.insert(captureBuffer.end(), pcm, pcm + take);
}

size_t Controller::drainCapturedAudio(int16_t *out, size_t cap)
{
    if (out == nullptr || cap == 0)
    {
        return 0;
    }

    std::lock_guard<std::mutex> lock(captureMutex);

    // Whole 16-bit samples only; a trailing odd byte (never expected) waits for its pair.
    size_t available = (captureBuffer.size() / sizeof(int16_t));
    size_t count = available < cap ? available : cap;
    size_t bytes = count * sizeof(int16_t);

    std::memcpy(out, captureBuffer.data(), bytes);
    captureBuffer.erase(captureBuffer.begin(), captureBuffer.begin() + bytes);
    return count;
}

uint8_t Controller::captureFormatCode() const
{
    // audioDeviceFormats is a flat list of (capture, render) code pairs; [0] is the capture
    // (microphone) format. Zero when no audio sub-device has announced.
    if (audioDeviceId == 0 || audioDeviceFormats.empty())
    {
        return 0;
    }
    return audioDeviceFormats[0];
}

/*
 * Whether the pad declared an audio format we can render to.
 *
 * Metadata lists these as (capture, render) pairs, which is how xone reads the same element -
 * it hands data[0] and data[1] straight to its format request. A pad that declared none has no
 * audio endpoint on this device id, and asking it to take audio is answered with silence.
 */
void Controller::audioStats(uint32_t out[6]) const
{
    out[0] = audioPacketsSent.load(std::memory_order_relaxed);
    out[1] = audioBytesDropped.load(std::memory_order_relaxed);
    out[2] = audioStarved.load(std::memory_order_relaxed);
    out[3] = audioSendFailures.load(std::memory_order_relaxed);
    out[4] = audioFlowRate.load(std::memory_order_relaxed);
    // Only a transport can see these: an isochronous packet reports its own status, where a GIP
    // message on the main link simply succeeds or fails as a whole.
    out[5] = audioTransport != nullptr ? audioTransport->underruns() : 0;
}

/*
 * One JNI void call on the read thread, which is what keeps this off the blocking path: the
 * teardown it triggers joins a sender thread that can sit in a USB write for a second, and this
 * thread is still carrying the pad's input reports while that happens.
 */
void Controller::notifyAudioDeviceRemoved()
{
    if (sink) {
        sink->onAudioRemoved(slot);
    }
}

void Controller::forgetAudioDevice()
{
    audioDeviceId = 0;
    audioDeviceFormats.clear();
    audioDeviceAnnounced = false;

    // Released last, so a report arriving mid-teardown finds audioDeviceId already zero and stops
    // on that test rather than starting a second teardown
    audioRemovalNotified.store(false);

    // Cleared too, or a jack re-inserted later would inherit the old device's rate as its baseline
    audioFlowRate.store(0, std::memory_order_relaxed);

    // Drop any mic PCM still held, so a re-inserted headset does not start with the old one's tail.
    {
        std::lock_guard<std::mutex> lock(captureMutex);
        captureBuffer.clear();
    }
}

bool Controller::supportsAudioOut() const
{
    /*
     * The audio sub-device's formats, not the pad's. A pad reports none of its own and is not
     * supposed to: audio is a separate GIP device with its own metadata, and it only announces
     * once the security handshake has completed (MS-GIPUSB 2.2.1.4). Asking device 0 was always
     * going to answer no, whatever was plugged into the jack.
     */
    if (audioDeviceId == 0 || audioDeviceFormats.size() < METADATA_AUDIO_FORMAT_LENGTH)
    {
        return false;
    }

    /*
     * The *first* pair only, because the first pair is what setAudioEnabled() will propose - 2.2.11
     * has the host take the device's first advertised configuration rather than choose among them.
     *
     * This used to scan every pair, which let the two disagree: a pad advertising 48 kHz stereo
     * anywhere in its list was offered in the menu, and then sent whatever its first pair happened
     * to be. Answering a question about pair 3 when pair 1 is the one that will be used is how a
     * menu ends up promising audio the stream cannot feed.
     *
     * Deliberately not fixed the other way round, by choosing the pair that suits us. AUDIO.md
     * records what that cost: proposing anything but the first pair retuned the render path to
     * 24 kHz mono and back on every connect, degrading the pad a step each time.
     *
     * Render is the second of each capture/render pair.
     */
    if (audioDeviceFormats[1] == AUDIO_FORMAT_48KHZ_STEREO)
    {
        return true;
    }

    /*
     * Said out loud, because no pad seen here reaches it. Both advertise 09/10 first, so this
     * branch is unexercised and would otherwise be a silent refusal on hardware nobody has - which
     * is exactly the kind of thing that gets debugged from the wrong end. If this ever prints, the
     * question to answer is whether 2.2.11's "first configuration" rule really is absolute.
     */
    Log::info("Audio device %u leads with format %02x/%02x, not 48 kHz stereo; not offering audio",
              (unsigned)audioDeviceId, audioDeviceFormats[0], audioDeviceFormats[1]);

    return false;
}

bool Controller::setAudioEnabled(bool enable)
{
    if (enable == audioEnabled)
    {
        return true;
    }

    if (enable)
    {
        /*
         * Refuse rather than half-succeed. Enabling routes the stream's audio away from the TV, so
         * a pad with nowhere to put it leaves the user with silence everywhere and no explanation.
         * The sub-device's metadata is what says, and it only exists once the security handshake
         * has completed (MS-GIPUSB 2.2.1.4).
         */
        if (audioDeviceId == 0 || audioDeviceFormats.size() < METADATA_AUDIO_FORMAT_LENGTH)
        {
            Log::info("No audio sub-device has announced; refusing audio");

            return false;
        }

        /*
         * Says which controller this is and how it will send.
         *
         * A controller with no transport falls back to sending GIP audio messages down the main
         * link, which on a cable is interface 0's interrupt endpoint: 16 KB/s against the 192 KB/s
         * audio needs. Two controllers once reported sessions for one cabled pad, and the one
         * without a transport discarded 65% of what it was given while writing into the same GIP
         * link the isochronous stream was using - two writers, one device, corrupted audio.
         *
         * So identity and send path are logged before anything starts, because from the outside a
         * duplicate looks exactly like a second pad.
         */
        Log::info("Audio enabling on controller %p, sub-device %u, transport %s",
                  static_cast<const void *>(this), (unsigned)audioDeviceId,
                  audioTransport != nullptr ? "isochronous" : "GIP link");

        /*
         * Before the format is proposed, because on a transport that carries audio itself the
         * endpoints have to exist before the device is told to start streaming into them. Refusing
         * here is the same trade as above: better no audio with a reason than the stream's audio
         * moved off the TV into silence.
         */
        if (audioTransport != nullptr && !audioTransport->enableAudio())
        {
            Log::error("Audio transport would not start; refusing audio");

            return false;
        }

        /*
         * Zeroed per session rather than per Controller, so a second enable reports its own
         * numbers instead of the sum of every one before it.
         */
        audioPacketsSent.store(0, std::memory_order_relaxed);
        audioBytesQueued.store(0, std::memory_order_relaxed);
        audioBytesDropped.store(0, std::memory_order_relaxed);
        audioStarved.store(0, std::memory_order_relaxed);
        audioSendFailures.store(0, std::memory_order_relaxed);

        /*
         * A transport that is already streaming is left entirely alone: the samples simply start
         * being real again.
         *
         * The device is configured once and then never renegotiated, which is both what 2.2.11
         * describes - "once started, audio data flows continually even if the data represents only
         * silence" - and what xone does, configuring at gip_headset_probe() and never again for
         * the life of the client. Neither ever asks a device to be reconfigured repeatedly.
         *
         * Doing it per session degraded the pad a little each time: first session clean, second
         * worse, third worse again, cleared only by unplugging, while our own side measured
         * perfect throughout - 192.2 bytes supplied per packet sent, no underruns, in the bad
         * sessions as much as the good one.
         *
         * What resuming does *not* do is re-enter the transport, so nothing here rebuilds the
         * ring's cushion - it has been empty for however long audio was off. The transport re-arms
         * itself on finding it empty for exactly this reason; without that, every session after the
         * first played from an empty ring. See WiredController::submitAudioTransfer().
         */
        if (audioTransport != nullptr && audioState == AUDIO_STREAMING)
        {
            // As the counters above are, and for the same reason: this session reports its own
            // numbers. The transport's are not cleared anywhere else, since it is never re-entered.
            audioTransport->resetStats();

            // The device is already streaming, so there is no volume message coming to open the
            // data plane; the transfer pools enableAudio() just rebuilt start here instead.
            if (!audioTransport->startStreaming())
            {
                Log::error("Audio transport would not resume streaming");
            }

            audioEnabled = true;

            Log::info("Audio: resuming into the running stream");

            return true;
        }

        /*
         * No RESET here, and that is deliberate rather than an omission.
         *
         * One was sent to the audio sub-device, followed by a 200 ms wait, to tear it down before
         * setting it up - a process that is killed sends no STOP, so the pad stays started and the
         * next session's configuration is issued to a device mid-stream. It never fixed that, and
         * 3.1.1 says why it could not: a device receiving RESET "MUST immediately reply with a GIP
         * Status that indicates powering off" and "SHOULD then wait 500 ms before a power off or
         * reset". A STOP arriving inside that window tells it to "immediately tear down all
         * sub-devices and then power off or reset from that point", and "no state change other than
         * the initial OFF or RESET is allowed".
         *
         * So the old sequence reset the sub-device, interrupted its teardown 200 ms in, and then
         * configured and started it within a few milliseconds more - while the specification has it
         * tearing down and going back to Arrival to send Hellos, ignoring exactly the messages we
         * were sending. It was left in after being measured as no cure, which is how the
         * discovery-time format retune survived too, and that one turned out to be causing the
         * pitch shift.
         *
         * What remains is 2.2.11's sequence and nothing else: STOP, configure, wait for the echo,
         * START, wait for the volume. That is also what the Windows capture does.
         */

        {
            std::lock_guard<std::mutex> lock(audioMutex);

            audioBuffer.clear();
            audioBuffer.reserve(AUDIO_BUFFER_MAX_BYTES);
        }

        audioFlowRate = 0;
        audioFlowMin = 0;
        audioFlowMax = 0;
        audioFlowChanges = 0;
        audioPacketsSent = 0;
        audioBytesDropped = 0;
        audioSendFailures = 0;
        audioStarved = 0;
        audioEnabled = true;

        /*
         * A sub-device that never announced is still configured from the last process, and adopting
         * that configuration is the whole point of this branch.
         *
         * 2.2.1 has a device send Hello only while in Arrival, so one left Active by a killed
         * process never announces. Nothing gets it back there: STOP does not move it, RESET
         * silences it for good, and re-enumerating costs input - all three tried on hardware, only
         * unplugging the cable works. What it *is*, though, is a device we configured ourselves
         * last session, to the same 09/10 we would propose now, and which discovery has since put
         * back in Idle with that format retained.
         *
         * So renegotiating asks it to change to what it already is, which is the one thing 2.2.11
         * and xone agree a host must not do - "once started, audio data flows continually", and
         * xone configures at gip_headset_probe() and never again. Every stuttering session measured
         * here did exactly that, while our own side reported 100% supply, bus-rate drain and no
         * underruns. So this sends START and nothing else, and lets the volume message start it.
         *
         * If the assumption is wrong the device simply says nothing, and waitForAudioVolume() falls
         * back to the full sequence - which is today's behaviour, three seconds later.
         */
        if (!audioDeviceAnnounced.load())
        {
            audioState = AUDIO_AWAITING_VOLUME;

            Log::info("Audio: device %u never announced; adopting its configuration",
                      audioDeviceId);

            if (!setDeviceState(audioDeviceId, STATE_START))
            {
                Log::error("Failed to start the adopted audio device");
            }

            startVolumeWait();

            return true;
        }

        if (!negotiateAudioFormat())
        {
            audioEnabled = false;

            return false;
        }

        // Sending begins once the device has echoed the format and reported its volume
        return true;
    }

    audioEnabled = false;

    /*
     * Stood down first: a wait still running would re-send START, and on the paused path below
     * would eventually renegotiate a device nobody is feeding.
     */
    {
        std::lock_guard<std::mutex> lock(volumeMutex);

        stopVolumeThread = true;
    }

    volumeCondition.notify_one();

    if (volumeThread.joinable())
    {
        volumeThread.join();
    }

    /*
     * The stream stays up and carries silence, for the same reason it is configured only once. The
     * ring stops being filled, drainAudio() finds it empty and pads, and the device sees exactly
     * what 2.2.11 says it should see when there is nothing to play.
     */
    if (audioTransport != nullptr && audioState == AUDIO_STREAMING)
    {
        Log::info("Audio session: %u packets sent, %u bytes queued, %u bytes dropped, %u late, "
                  "%u send failures, %u underruns, flow rate %u (%u-%u, %u changes)",
                  audioPacketsSent.load(std::memory_order_relaxed),
                  audioBytesQueued.load(std::memory_order_relaxed),
                  audioBytesDropped.load(std::memory_order_relaxed),
                  audioStarved.load(std::memory_order_relaxed),
                  audioSendFailures.load(std::memory_order_relaxed),
                  audioTransport->underruns(),
                  (unsigned)audioFlowRate.load(std::memory_order_relaxed),
                  (unsigned)audioFlowMin.load(std::memory_order_relaxed),
                  (unsigned)audioFlowMax.load(std::memory_order_relaxed),
                  audioFlowChanges.load(std::memory_order_relaxed));

        Log::info("Audio paused; the stream stays up carrying silence");

        return true;
    }

    audioState = AUDIO_IDLE;
    stopAudioThread = true;
    audioCondition.notify_one();

    if (audioThread.joinable())
    {
        audioThread.join();
    }

    if (audioDeviceId != 0)
    {
        setDeviceState(audioDeviceId, STATE_STOP);
    }

    {
        std::lock_guard<std::mutex> lock(audioMutex);

        audioBuffer.clear();
        audioBuffer.shrink_to_fit();
    }

    /*
     * The session's totals, reported where they will actually be read. CLAUDE.md's note about the
     * end-of-stream summary applies: this is what reaches a bug report, whereas anything only on
     * screen is gone the moment the stream ends.
     *
     * Expect packets at 125 a second - one per 8 ms - so the count against the time audio was on
     * says whether the stream kept up. Dropped bytes mean the host outran the link and audio would
     * have drifted; starved counts mean the reverse, a ring emptied and a gap heard.
     */
    /*
     * Flow rate is what the device asked for last, from the Audio Capture messages on its
     * isochronous IN endpoint - the mechanism 3.2.5.1.5 calls the way GIP devices eliminate pops.
     * A zero means it never reported one, which for a cabled pad means the capture endpoint went
     * unread, so it is logged rather than hidden.
     */
    Log::info("Audio session: %u packets sent, %u bytes queued, %u bytes dropped, %u late, "
              "%u send failures, %u underruns, flow rate %u (%u-%u, %u changes)",
              audioPacketsSent.load(std::memory_order_relaxed),
              audioBytesQueued.load(std::memory_order_relaxed),
              audioBytesDropped.load(std::memory_order_relaxed),
              audioStarved.load(std::memory_order_relaxed),
              audioSendFailures.load(std::memory_order_relaxed),
              audioTransport != nullptr ? audioTransport->underruns() : 0,
              (unsigned)audioFlowRate,
              (unsigned)audioFlowMin.load(std::memory_order_relaxed),
              (unsigned)audioFlowMax.load(std::memory_order_relaxed),
              audioFlowChanges.load(std::memory_order_relaxed));

    if (audioTransport != nullptr)
    {
        audioTransport->disableAudio();
    }

    Log::info("Audio disabled for controller");

    return true;
}

bool Controller::negotiateAudioFormat()
{
    /*
     * 2.2.11's sequence, and the order is the point: stop the device, propose a format, and wait.
     * The host does not choose a format - it takes the first pair the device advertised, capture
     * then render. Sending a format and streaming immediately, which is what this did before, skips
     * three steps the device is waiting on.
     */
    if (!setDeviceState(audioDeviceId, STATE_STOP))
    {
        Log::error("Failed to stop the audio device");

        return false;
    }

    auto in = static_cast<AudioFormat>(audioDeviceFormats[0]);
    auto out = static_cast<AudioFormat>(audioDeviceFormats[1]);

    if (!setAudioFormat(audioDeviceId, in, out))
    {
        Log::error("Failed to propose an audio format");

        return false;
    }

    audioState = AUDIO_AWAITING_FORMAT;

    Log::info("Audio: proposed format %02x/%02x to device %u, awaiting its reply",
              in, out, audioDeviceId);

    return true;
}

void Controller::startVolumeWait()
{
    /*
     * One waiter at a time. The previous one has already finished if the state moved on, and
     * joining it here is what makes that true before a second is created.
     */
    {
        std::lock_guard<std::mutex> lock(volumeMutex);

        stopVolumeThread = true;
    }

    volumeCondition.notify_one();

    if (volumeThread.joinable())
    {
        volumeThread.join();
    }

    {
        std::lock_guard<std::mutex> lock(volumeMutex);

        stopVolumeThread = false;
    }

    volumeThread = std::thread(&Controller::waitForAudioVolume, this);
}

void Controller::waitForAudioVolume()
{
    /*
     * 2.2.11: "The host sends Set Device State: START at 500 ms intervals until receipt of the
     * volume message, or until it times out after 3 seconds." START was sent once and waited on
     * forever, so a lost START or a device that never answered left audio enabled and silent with
     * nothing to say why.
     *
     * Sunburst: nothing on this thread calls into Java any more, so it does not
     * attach to the JVM.
     */
    // Six intervals of 500 ms is the specification's three seconds, counted in the units it gives.
    for (int attempt = 0; attempt < AUDIO_VOLUME_ATTEMPTS; attempt++)
    {
        std::unique_lock<std::mutex> lock(volumeMutex);

        volumeCondition.wait_for(lock, std::chrono::milliseconds(AUDIO_VOLUME_INTERVAL_MS),
                                 [this] {
                                     return stopVolumeThread
                                         || audioState != AUDIO_AWAITING_VOLUME;
                                 });

        if (stopVolumeThread || audioState != AUDIO_AWAITING_VOLUME)
        {
            return;
        }

        lock.unlock();

        // Resent rather than merely waited on, which is the half that was missing
        if (!setDeviceState(audioDeviceId, STATE_START))
        {
            Log::error("Failed to re-send start to the audio device");
        }
    }

    /*
     * Three seconds with no volume message. On a device we adopted, that is the adoption being
     * wrong - it was not configured the way we assumed - so fall back to configuring it properly,
     * which is what every session did before adopting existed.
     */
    Log::info("Audio: no volume after %d ms; renegotiating the format",
              AUDIO_VOLUME_ATTEMPTS * AUDIO_VOLUME_INTERVAL_MS);

    if (!negotiateAudioFormat())
    {
        Log::error("Failed to renegotiate the audio format");
    }
}

/*
 * Drives the rest of 2.2.11's sequence, which is answer-driven rather than timed.
 *
 * The device echoes the format it actually adopted; only then may it be started. After starting it
 * sends a volume message, and the specification is explicit that the host "might not start to play
 * audio data until a volume indication is received" - so that message, not the start, is what opens
 * the stream.
 */
void Controller::audioControlReceived(uint8_t id, const uint8_t *data, size_t length)
{
    if (id != audioDeviceId || length < 1)
    {
        return;
    }

    uint8_t subcommand = data[0];

    if (subcommand == AUDIO_CTRL_FORMAT && length >= 3)
    {
        if (audioState != AUDIO_AWAITING_FORMAT)
        {
            return;
        }

        // A device that cannot manage what was proposed answers with one it can, from its own
        // list. Nothing here renegotiates yet; say so rather than starting it on a mismatch.
        if (data[1] != audioDeviceFormats[0] || data[2] != audioDeviceFormats[1])
        {
            Log::error("Audio: device adopted %02x/%02x, not the %02x/%02x proposed",
                       data[1], data[2], audioDeviceFormats[0], audioDeviceFormats[1]);

            return;
        }

        Log::info("Audio: device confirmed format %02x/%02x, starting it", data[1], data[2]);

        audioState = AUDIO_AWAITING_VOLUME;

        if (!setDeviceState(audioDeviceId, STATE_START))
        {
            Log::error("Failed to start the audio device");
        }

        // 2.2.11 has the host repeat START until the volume arrives, on this path as much as on
        // the adopted one, so both wait the same way rather than only the new one being covered.
        startVolumeWait();

        return;
    }

    if (subcommand == AUDIO_CTRL_VOLUME)
    {
        /*
         * Record what the device says before anything else, and do it whenever it arrives rather
         * than only during startup - 3.2.5.1.1 has the device re-send this whenever a field
         * changes, which is how a volume control on the device itself would reach us.
         */
        if (length >= sizeof(AudioVolumeData))
        {
            memcpy(&audioVolumeReported, data, sizeof(AudioVolumeData));
            audioVolumeKnown = true;

            Log::info(
                "Audio: device volume, speaker %u%% (%s), balance %u, mic %u%%, flags 0x%02x",
                audioVolumeReported.speaker & AUDIO_VOLUME_LEVEL,
                (audioVolumeReported.speaker & AUDIO_VOLUME_WRITABLE) ? "writable" : "read-only",
                audioVolumeReported.balance & AUDIO_VOLUME_LEVEL,
                audioVolumeReported.microphone & AUDIO_VOLUME_LEVEL,
                audioVolumeReported.flags
            );
        }
        else
        {
            // The plain Volume message, which the specification names but never gives a layout
            // for. Nothing can be read from it, so volume falls back to software scaling.
            Log::info("Audio: device reported volume in %zu bytes, not the extended form", length);
        }

        /*
         * Adopt the device's own level unless the user has chosen one. A pad here comes up at 80%,
         * so assuming 100 both misreported it in the menu and made "select 100%" audibly raise the
         * volume from a value that claimed to already be 100.
         *
         * Re-applying the user's choice on every volume message is deliberate: 3.2.5.1.1 has the
         * device re-send this whenever a field changes, so this is also what puts the level back
         * if something else moved it.
         */
        if (audioVolumeChosen)
        {
            setAudioVolume(audioVolumePercent);
        }
        else if (audioVolumeKnown)
        {
            audioVolumePercent = audioVolumeReported.speaker & AUDIO_VOLUME_LEVEL;
        }

        if (audioState != AUDIO_AWAITING_VOLUME)
        {
            return;
        }

        Log::info("Audio: device reported volume, streaming");

        audioState = AUDIO_STREAMING;
        stopAudioThread = false;

        // Ends the START retry now rather than on its next tick; the predicate reads audioState,
        // which has just changed, so this only saves the wait rather than deciding anything.
        volumeCondition.notify_one();

        /*
         * Only now does anything touch the isochronous endpoints. The volume message is 2.2.11's
         * signal that the device is ready - xone gates its first URBs on it, and the Windows
         * capture starts render 31 ms after it - and this driver used to ignore that line,
         * spraying silence into the endpoint from the moment the transport came up, before the
         * device had even been stopped and configured.
         */
        if (audioTransport != nullptr && !audioTransport->startStreaming())
        {
            Log::error("Audio transport would not start streaming");
        }

        /*
         * Only where we are the clock. A transport that carries audio itself is paced by its own
         * bus and pulls through drainAudio() from its completion callbacks, so a sender thread
         * pushing at the host's rate would be a second, disagreeing clock on the same ring.
         */
        if (audioTransport == nullptr)
        {
            audioThread = std::thread(&Controller::processAudio, this);
        }
    }
}

void Controller::begin()
{
    /*
     * For a transport whose device never announces. A cabled pad has already been through Arrival
     * by the time we claim it, and MS-GIPUSB 2.2.1 has a device Hello only while in Arrival, so
     * waiting for one waits forever.
     *
     * Set Device State: RESET was tried here first, on the reasoning that 3.1.1 makes it a
     * protocol-level return to Arrival. On this hardware it takes the device off the USB bus
     * entirely - the read fails with NO_DEVICE and Android re-attaches it, permission prompt and
     * all, round and round. So nothing is reset and nothing is restarted: the device is simply
     * asked for its metadata where it stands, which is the least it can be sent.
     */
    if (inputInitialised.exchange(true))
    {
        return;
    }

    initInput();
}

size_t Controller::audioRenderBytes() const
{
    // One millisecond of 48 kHz 16-bit stereo, which is what the device asks for when its buffer
    // is level and what it modulates either side of.
    const size_t nominal = AUDIO_BYTES_PER_SECOND / 1000;

    uint16_t requested = audioFlowRate.load(std::memory_order_relaxed);

    if (requested == 0)
    {
        return nominal;
    }

    /*
     * Clamped to the band 3.2.5.1.5 describes. A value outside it is not rate adaptation - it is a
     * misparse or a device asking for a configuration we never negotiated - and following it would
     * turn a small drift into a large one.
     */
    const size_t margin = 2 * 2 * 2;

    if (requested < nominal - margin || requested > nominal + margin)
    {
        return nominal;
    }

    return requested;
}

size_t Controller::audioBuffered()
{
    std::lock_guard<std::mutex> lock(audioMutex);

    return audioBuffer.size();
}

size_t Controller::drainAudio(uint8_t *out, size_t length)
{
    size_t taken = 0;

    {
        std::lock_guard<std::mutex> lock(audioMutex);

        taken = std::min(length, audioBuffer.size());

        std::copy(audioBuffer.begin(), audioBuffer.begin() + taken, out);
        audioBuffer.erase(audioBuffer.begin(), audioBuffer.begin() + taken);
    }

    // Silence rather than stale samples: the endpoint sends this millisecond either way, and a
    // repeat of the last one is a click where a gap is merely quiet.
    if (taken < length)
    {
        std::fill(out + taken, out + length, 0);

        audioStarved.fetch_add(1, std::memory_order_relaxed);
    }

    // Only where the device refused to attenuate for us; see setAudioVolume()
    uint16_t scale = audioSoftwareScale.load(std::memory_order_relaxed);

    if (scale != 256 && taken > 0)
    {
        auto *samples = reinterpret_cast<int16_t *>(out);

        for (size_t i = 0; i < taken / sizeof(int16_t); i++)
        {
            samples[i] = static_cast<int16_t>((samples[i] * scale) >> 8);
        }
    }

    audioPacketsSent.fetch_add(1, std::memory_order_relaxed);

    return taken;
}

void Controller::audioFlowRateReported(uint16_t flowRate)
{
    if (flowRate == audioFlowRate.load(std::memory_order_relaxed))
    {
        return;
    }

    audioFlowRate.store(flowRate, std::memory_order_relaxed);

    if (flowRate == 0)
    {
        return;
    }

    /*
     * Single writer - only the capture path reaches this - so read-modify-write on relaxed
     * atomics is race-free, and it only runs on a change, which 3.2.5.1.5 expects to be rare.
     */
    uint16_t seen = audioFlowMin.load(std::memory_order_relaxed);

    if (seen == 0 || flowRate < seen)
    {
        audioFlowMin.store(flowRate, std::memory_order_relaxed);
    }

    if (flowRate > audioFlowMax.load(std::memory_order_relaxed))
    {
        audioFlowMax.store(flowRate, std::memory_order_relaxed);
    }

    audioFlowChanges.fetch_add(1, std::memory_order_relaxed);
}

void Controller::setAudioTransport(GipAudioTransport *transport)
{
    audioTransport = transport;
}

size_t Controller::encodeAudioFragment(const uint8_t *samples, size_t length, uint8_t *out)
{
    return encodeAudioMessage(audioDeviceId, samples, length, out);
}

void Controller::setAudioPacketBytes(size_t bytes)
{
    if (bytes == 0)
    {
        return;
    }

    audioPacketBytes = bytes;
}

bool Controller::setAudioVolume(uint8_t percent)
{
    if (percent > 100)
    {
        percent = 100;
    }

    audioVolumePercent = percent;
    audioVolumeChosen = true;

    // The device does the attenuation itself where it will let us ask, which is both free and what
    // 3.2.5.1.1 intends. Only the speaker field is touched: chat balance and microphone are not
    // ours to move, and this client has no microphone at all.
    if (audioVolumeKnown && (audioVolumeReported.speaker & AUDIO_VOLUME_WRITABLE))
    {
        audioSoftwareScale.store(256, std::memory_order_relaxed);

        AudioVolumeData volume = audioVolumeReported;

        volume.subcommand = AUDIO_CTRL_VOLUME;
        volume.speaker = AUDIO_VOLUME_WRITABLE | percent;

        return GipDevice::setAudioVolume(audioDeviceId, volume);
    }

    /*
     * Nothing to ask, so attenuate the samples ourselves. 8.8 fixed point, so 100% lands exactly on
     * unity and skips the scaling loop rather than multiplying every sample by 1.
     */
    audioSoftwareScale.store(static_cast<uint16_t>((percent * 256) / 100),
                             std::memory_order_relaxed);

    if (audioVolumeKnown)
    {
        Log::info("Audio: device volume is read-only, scaling in software to %u%%", percent);
    }

    return true;
}

void Controller::queueAudio(const int16_t *samples, size_t count)
{
    if (!audioEnabled)
    {
        return;
    }

    const uint8_t *bytes = reinterpret_cast<const uint8_t *>(samples);
    size_t length = count * sizeof(int16_t);

    audioBytesQueued.fetch_add(static_cast<uint32_t>(length), std::memory_order_relaxed);

    {
        std::lock_guard<std::mutex> lock(audioMutex);

        // Drop the oldest rather than growing without bound. A backlog here is heard as audio
        // drifting permanently behind the video, which is worse than losing a few milliseconds -
        // the same trade AudioRenderer's own documentation describes for the AudioTrack path.
        if (audioBuffer.size() + length > AUDIO_BUFFER_MAX_BYTES)
        {
            size_t excess = audioBuffer.size() + length - AUDIO_BUFFER_MAX_BYTES;

            audioBytesDropped.fetch_add(static_cast<uint32_t>(excess), std::memory_order_relaxed);

            if (excess >= audioBuffer.size())
            {
                audioBuffer.clear();
            }
            else
            {
                audioBuffer.erase(audioBuffer.begin(), audioBuffer.begin() + excess);
            }
        }

        audioBuffer.insert(audioBuffer.end(), bytes, bytes + length);
    }

    audioCondition.notify_one();
}

/*
 * Drains the ring one packet at a time. There is no timer: the host delivers audio in real time,
 * so waiting for a packet's worth of samples paces this at the protocol's 8 ms on its own.
 */
void Controller::processAudio()
{
    // Sized once here rather than per packet: the loop below must not allocate, and the size is
    // fixed for the life of the sender because a transport does not change mid-session.
    std::vector<uint8_t> packet(audioPacketBytes);

    while (!stopAudioThread)
    {
        {
            std::unique_lock<std::mutex> lock(audioMutex);

            auto ready = [this] {
                return stopAudioThread || audioBuffer.size() >= audioPacketBytes;
            };

            /*
             * Waiting is normal and says nothing: the ring is how this paces itself, so the sender
             * waits on every packet by design, and at a perfectly matched rate it finds the ring
             * empty every time. Waiting *longer than the cadence* is the thing worth counting -
             * that is the host failing to supply in time, which is what a gap sounds like.
             *
             * An earlier version counted an empty ring instead and reported 20% starvation through
             * audio that was audibly fine.
             */
            if (!audioCondition.wait_for(lock, AUDIO_STARVE_TIMEOUT, ready))
            {
                audioStarved.fetch_add(1, std::memory_order_relaxed);

                audioCondition.wait(lock, ready);
            }

            if (stopAudioThread)
            {
                return;
            }

            std::copy(audioBuffer.begin(), audioBuffer.begin() + audioPacketBytes, packet.begin());
            audioBuffer.erase(audioBuffer.begin(), audioBuffer.begin() + audioPacketBytes);
        }

        /*
         * Only reached on a device that refuses host volume requests; setAudioVolume() leaves this
         * at unity otherwise, so the usual cost is one relaxed load and a comparison per 8 ms
         * packet rather than anything per sample.
         */
        uint16_t scale = audioSoftwareScale.load(std::memory_order_relaxed);

        if (scale != 256)
        {
            int16_t *samples = reinterpret_cast<int16_t *>(packet.data());

            for (size_t i = 0; i < packet.size() / sizeof(int16_t); i++)
            {
                samples[i] = static_cast<int16_t>((samples[i] * scale) >> 8);
            }
        }

        /*
         * Outside the lock either way: the adapter's send is a blocking USB transfer and the
         * decode thread must never wait behind it, and a transport's submit can block on a free
         * buffer for the same reason.
         */
        bool sent = audioTransport != nullptr
                        ? audioTransport->sendAudio(packet.data(), packet.size())
                        : sendAudioSamples(audioDeviceId, packet.data(), packet.size());

        if (!sent)
        {
            audioSendFailures.fetch_add(1, std::memory_order_relaxed);

            Log::error("Failed to send audio samples");

            return;
        }

        audioPacketsSent.fetch_add(1, std::memory_order_relaxed);
    }
}

void Controller::processRumble()
{
    RumbleData rumble = {};
    std::unique_lock<std::mutex> lock(rumbleMutex);

    while (!stopRumbleThread)
    {
        rumbleCondition.wait(lock);

        while (rumbleBuffer.get(rumble))
        {
            performRumble(rumble);

            // Delay rumble to work around firmware bug
            std::this_thread::sleep_for(RUMBLE_DELAY);
        }
    }
}

void Controller::sendRumble() {
    RumbleData rumble = {};

    rumble.setLeft = true;
    rumble.setRight = true;
    rumble.setLeftTrigger = true;
    rumble.setRightTrigger = true;
    rumble.left = RUMBLE_SCALE(this->rumbleLeft);
    rumble.right = RUMBLE_SCALE(this->rumbleRight);
    rumble.leftTrigger = RUMBLE_SCALE(this->rumbleTriggerLeft);
    rumble.rightTrigger = RUMBLE_SCALE(this->rumbleTriggerRight);
    rumble.duration = 10;
    rumble.repeat = 1;

    rumbleBuffer.put(rumble);
    rumbleCondition.notify_one();
}

void Controller::inputRumble(short lowFreqMotor, short highFreqMotor) {
    this->rumbleLeft = lowFreqMotor;
    this->rumbleRight = highFreqMotor;

    sendRumble();
}

void Controller::inputRumbleTrigger(short leftTrigger, short rightTrigger) {
    this->rumbleTriggerLeft = leftTrigger;
    this->rumbleTriggerRight = rightTrigger;

    sendRumble();
}

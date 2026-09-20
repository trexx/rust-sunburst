/*
 * Copyright (C) 2020 Medusalix
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

#include "gip.h"
#include "../utils/log.h"
#include "../utils/bytes.h"
#include "../utils/crypto.h"

#include <algorithm>
#include <cstring>

enum FrameCommand
{
    CMD_ACKNOWLEDGE = 0x01,
    CMD_ANNOUNCE = 0x02,
    CMD_STATUS = 0x03,
    CMD_IDENTIFY = 0x04,
    CMD_SET_DEVICE_STATE = 0x05,
    CMD_AUTHENTICATE = 0x06,
    CMD_GUIDE_BTN = 0x07,
    CMD_AUDIO_CONFIG = 0x08,
    CMD_RUMBLE = 0x09,
    CMD_LED_MODE = 0x0a,
    CMD_EXTENDED = 0x1e,
    CMD_INPUT = 0x20,
    CMD_AUDIO_SAMPLES = 0x60,
};

// Different frame types
// Command: controller doesn't respond
// Request: controller responds with data
// Request (ACK): controller responds with ack + data
//
// These are the high nibble of the GIP header's flags byte (MS-GIPUSB 2.2.10.2), so the values
// here are the spec's bits shifted down by four: ACME is bit 4, System bit 5, InitFrag bit 6 and
// Fragment bit 7. The two original names predate the specification being published and are kept
// because they are what the rest of this driver says.
enum FrameType
{
    TYPE_COMMAND = 0x00,
    TYPE_ACK = 0x01,          // spec: ACME, "acknowledge me"
    TYPE_REQUEST = 0x02,      // spec: System
    TYPE_CHUNK_START = 0x04,  // spec: InitFrag, first fragment of a fragmented message
    TYPE_CHUNK = 0x08,        // spec: Fragment
};

// The largest metadata or security transfer we will reassemble. The spec caps a message at what
// its data class MTU and the length encoding allow; this is a sanity bound so a corrupt length
// cannot make us allocate wildly. xone uses the same value.
#define CHUNK_BUFFER_MAX_LENGTH 0xffff

// Command, flags and sequence, plus a four-byte length at most, plus a byte of even-length padding
#define HEADER_MAX_LENGTH 8

struct Frame
{
    uint8_t command;
    uint8_t deviceId : 4;
    uint8_t type : 4;
    uint8_t sequence;
    uint8_t length;
} __attribute__((packed));

/*
 * Payload of a Protocol Control acknowledgement (MS-GIPUSB 3.1.5.1). 'received' is how much of the
 * message has arrived contiguously and 'remaining' how much is still expected, which is what lets
 * the sender decide whether to retransmit.
 *
 * acknowledgePacket() below builds the same nine bytes by reusing Frame, which happens to lay out
 * identically; this spells the fields out because the chunked path has real values to put in them.
 */
struct AcknowledgeData
{
    uint8_t unknown;
    uint8_t command;
    uint8_t options;
    uint16_t received;
    uint8_t padding[2];
    uint16_t remaining;
} __attribute__((packed));

/*
 * Decodes the variable-length integer the header uses for payload length and chunk offset
 * (MS-GIPUSB 3.1.5.2): seven bits per byte, low order first, bit 7 meaning another byte follows.
 * At most four bytes.
 *
 * Returns the number of bytes consumed, or zero if there were none to read.
 */
static size_t decodeVarint(const uint8_t *data, size_t available, uint32_t &value)
{
    size_t i;

    value = 0;

    for (i = 0; i < sizeof(value) && i < available; i++)
    {
        value |= static_cast<uint32_t>(data[i] & 0x7f) << (i * 7);

        if (!(data[i] & 0x80))
        {
            return i + 1;
        }
    }

    // Ran out of input, or the length claimed more continuation bytes than the encoding allows
    return 0;
}

/*
 * The inverse of decodeVarint(). Returns the number of bytes written, at most four.
 */
static size_t encodeVarint(uint8_t *buf, uint32_t value)
{
    size_t i;

    for (i = 0; i < sizeof(value); i++)
    {
        buf[i] = value & 0x7f;
        value >>= 7;

        if (value == 0)
        {
            return i + 1;
        }

        buf[i] |= 0x80;
    }

    return i;
}

/*
 * Builds a fragment header: command, flags, sequence, then the length and the offset, each a
 * varint.
 *
 * On the first fragment the offset field carries the *total* message length rather than an offset -
 * the specification calls it TLO for that reason - and later fragments put their own byte offset
 * there.
 *
 * The header must come to an even number of bytes. Padding goes on the length varint, not after the
 * offset, because the offset has to be last for the reader: set the continuation bit on the length's
 * final byte and insert a zero, which reads back as the same value. A captured host does exactly
 * this - a 58-byte length appears as "ba 00" when the offset needs only one byte.
 *
 * Returns the header length; the buffer needs room for HEADER_MAX_LENGTH.
 */
static size_t encodeChunkHeader(uint8_t *buf, uint8_t command, uint8_t deviceId, uint8_t type,
                                uint8_t sequence, uint32_t length, uint32_t offset)
{
    uint8_t lengthBytes[5];
    uint8_t offsetBytes[5];

    size_t lengthUsed = encodeVarint(lengthBytes, length);
    size_t offsetUsed = encodeVarint(offsetBytes, offset);

    if ((3 + lengthUsed + offsetUsed) % 2)
    {
        lengthBytes[lengthUsed - 1] |= 0x80;
        lengthBytes[lengthUsed++] = 0;
    }

    size_t used = 0;

    buf[used++] = command;
    buf[used++] = static_cast<uint8_t>((type << 4) | (deviceId & 0x0f));
    buf[used++] = sequence;

    std::copy(lengthBytes, lengthBytes + lengthUsed, buf + used);
    used += lengthUsed;

    std::copy(offsetBytes, offsetBytes + offsetUsed, buf + used);
    used += offsetUsed;

    return used;
}

/*
 * Builds a downstream header, using the extended length encoding when the payload needs it.
 *
 * The single length byte in Frame tops out at 127, and a handshake message carrying a 256-byte
 * encrypted secret is far past that - casting its length into a byte sends 274 as 18, which the
 * device reads as a malformed message and answers by dropping the link.
 *
 * Downstream headers must also be an even number of bytes (MS-GIPUSB 3.1.5.2). An odd one is padded
 * the way xone does it: set the continuation bit on the last length byte and append a zero, which
 * reads back as the same value.
 *
 * Returns the header length; the buffer needs room for HEADER_MAX_LENGTH.
 */
static size_t encodeHeader(uint8_t *buf, uint8_t command, uint8_t deviceId, uint8_t type,
                           uint8_t sequence, uint32_t length)
{
    size_t used = 0;

    buf[used++] = command;
    buf[used++] = static_cast<uint8_t>((type << 4) | (deviceId & 0x0f));
    buf[used++] = sequence;

    used += encodeVarint(buf + used, length);

    if (used % 2)
    {
        buf[used - 1] |= 0x80;
        buf[used++] = 0;
    }

    return used;
}

GipDevice::GipDevice(SendPacket sendPacket) : sendPacket(sendPacket)
{
#ifdef _DEBUG
    // Printed so that an empty accessory log can be read as "nothing arrived" rather than "the
    // diagnostics were compiled out". Those look identical afterwards, and telling them apart
    // after the fact is not possible.
    Log::info("Accessory diagnostics compiled in");
#endif
}

#ifdef _DEBUG
void GipDevice::logAccessoryPacket(const Frame &frame, size_t size, const char *where)
{
    // First twenty, then one in two hundred. The question is whether anything arrives at all, not
    // at what rate, and an accessory streaming audio would match the input report rate.
    if (accessoryPackets < 20 || accessoryPackets % 200 == 0)
    {
        Log::info("Accessory %s: #%u id=%u cmd=%02x ty=%u len=%u size=%zu",
                  where, accessoryPackets, (unsigned)frame.deviceId, (unsigned)frame.command,
                  (unsigned)frame.type, (unsigned)frame.length, size);
    }

    accessoryPackets++;
}
#endif

bool GipDevice::handlePacket(const Bytes &packet)
{
    // Ignore invalid packets
    if (packet.size() < sizeof(Frame))
    {
        return true;
    }

    const Frame *frame = packet.toStruct<Frame>();

    // Fragmented messages carry a longer header - a multi-byte length and a total-length-or-offset
    // field after it - so they cannot be read through the fixed Frame struct. Only metadata and
    // security arrive this way; input, status and the rest never do, so the common path below is
    // left exactly as it was rather than routed through the reassembly code.
    if (frame->type & (TYPE_CHUNK | TYPE_CHUNK_START))
    {
#ifdef _DEBUG
        // Logged here as well as at the accessory filter below, because this branch runs first:
        // a fragmented accessory message is reassembled as though it belonged to device 0, and
        // dispatchChunked() would hand it to identifyReceived(). Worth knowing whether that ever
        // happens before deciding whether it needs fixing.
        if (frame->deviceId > 0)
        {
            logAccessoryPacket(*frame, packet.size(), "chunk");
        }
#endif

        const uint8_t *raw = packet.raw();
        // Command, flags and sequence are fixed width; the length field starts after them
        size_t offset = 3;
        uint32_t length = 0;
        uint32_t chunkOffset = 0;

        size_t consumed = decodeVarint(raw + offset, packet.size() - offset, length);
        if (consumed == 0)
        {
            Log::debug("Malformed chunk length");

            return true;
        }
        offset += consumed;

        if (frame->type & TYPE_CHUNK)
        {
            consumed = decodeVarint(raw + offset, packet.size() - offset, chunkOffset);
            if (consumed == 0)
            {
                Log::debug("Malformed chunk offset");

                return true;
            }
            offset += consumed;
        }

        if (packet.size() < offset + length)
        {
            Log::debug("Truncated chunk payload");

            return true;
        }

        return handleChunk(*frame, length, chunkOffset, Bytes(packet, offset));
    }

    if (frame->type & TYPE_ACK && !acknowledgePacket(*frame))
    {
        Log::error("Failed to acknowledge packet");

        return false;
    }

#ifdef _DEBUG
    if (frame->deviceId > 0)
    {
        logAccessoryPacket(*frame, packet.size(), "packet");
    }
#endif


    // An unfragmented message may still carry the multi-byte length encoding: the single byte in
    // Frame tops out at 127, and MS-GIPUSB 2.2.10.4 has anything longer use the varint form with
    // the continuation bit set. Nothing this driver handles today is that long - input, status,
    // announce, guide and serial are all well under it. Audio is the first message that needs it:
    // a 48 kHz stereo packet is 1536 bytes. Reading such a header through Frame alone would take
    // the first length byte literally and dispatch against a wrong length.
    uint32_t payloadLength = frame->length;
    size_t headerLength = sizeof(Frame);

    if (frame->length & 0x80)
    {
        size_t consumed = decodeVarint(packet.raw() + 3, packet.size() - 3, payloadLength);

        if (consumed == 0)
        {
            Log::debug("Malformed payload length");

            return true;
        }

        headerLength = 3 + consumed;

        if (packet.size() < headerLength)
        {
            return true;
        }
    }

    const Bytes data(packet, headerLength);

    // Data is 32-bit aligned, check for minimum size
    if (
        frame->command == CMD_ANNOUNCE &&
        payloadLength == sizeof(AnnounceData) &&
        data.size() >= sizeof(AnnounceData)
    ) {
        deviceAnnounced(
            frame->deviceId,
            data.toStruct<AnnounceData>()
        );
    }

    // Newer controllers send a larger status packet
    // MS-GIPUSB Table 26 allows three status payload sizes: 0x01 (legacy, deprecated), 0x04, and
    // 0x23-0x37 (extended). Section 3.1.5.5.2.2 says all new GIP devices MUST use the extended
    // form, so testing for exactly 0x04 dropped their status messages whole - no battery, no fault
    // events. The leading four bytes are the same in both, so parse those and ignore the tail.
    else if (
        frame->command == CMD_STATUS &&
        payloadLength >= sizeof(StatusData) &&
        data.size() >= sizeof(StatusData)
    ) {
        statusReceived(
            frame->deviceId,
            data.toStruct<StatusData>()
        );
    }

    else if (
        frame->command == CMD_GUIDE_BTN &&
        payloadLength == sizeof(GuideButtonData) &&
        data.size() >= sizeof(GuideButtonData)
    ) {
        guideButtonPressed(frame->deviceId, data.toStruct<GuideButtonData>());
    }

    else if (
        frame->command == CMD_AUTHENTICATE &&
        data.size() >= payloadLength
    ) {
        handleAuthPacket(data.raw(), payloadLength);
    }

    else if (
        frame->command == CMD_EXTENDED &&
        payloadLength == sizeof(SerialData) &&
        data.size() >= sizeof(SerialData)
    ) {
        serialNumberReceived(frame->deviceId, data.toStruct<SerialData>());
    }

    // Elite controllers send a larger input packet
    // The button remapping is done in hardware
    // The "non-remapped" input is appended to the packet
    else if (
        frame->command == CMD_INPUT &&
        payloadLength >= sizeof(InputData) &&
        data.size() >= sizeof(InputData)
    ) {
        inputReceived(frame->deviceId, data.toStruct<InputData>());
    }

    // The pad sends these whether or not it has a microphone, because the flow rate they carry is
    // how GIP does rate adaptation (MS-GIPUSB 3.2.5.1.5). Any mic samples after it are discarded -
    // this client has no microphone support.
    else if (
        frame->command == CMD_AUDIO_CONFIG &&
        payloadLength >= 1 &&
        data.size() >= payloadLength
    ) {
        audioControlReceived(frame->deviceId, data.raw(), payloadLength);
    }

    else if (
        frame->command == CMD_AUDIO_SAMPLES &&
        payloadLength >= sizeof(AudioSamplesData) &&
        data.size() >= sizeof(AudioSamplesData)
    ) {
        // The mic PCM follows the flow rate; empty when the pad has no microphone.
        const size_t off = sizeof(AudioSamplesData);
        const uint8_t *pcm = data.raw() + off;
        const size_t pcmLen = data.size() > off ? data.size() - off : 0;
        audioSamplesReceived(frame->deviceId, data.toStruct<AudioSamplesData>(), pcm, pcmLen);
    }

    /*
     * Metadata that fitted in one message, which until this existed was dropped here without
     * trace.
     *
     * Fragmentation is a function of size, not of message type (2.2.10.4): a device whose
     * metadata exceeds the Command class MTU sends it fragmented, and one whose metadata fits
     * sends it whole. Only the fragmented form was ever handled, in dispatchChunked(), because
     * every device measured until now took it - a pad's own metadata is 452 bytes and its
     * non-audio sub-device's 161.
     *
     * An Elite Series 2's 3.5 mm audio sub-device answers in 110 bytes, unfragmented, and so
     * never reached identifyReceived(). The audio device was therefore never adopted, its
     * formats never read, and headphone audio was unreachable on that pad - the menu offered
     * "Unavailable (no headset detected)" with the headset plugged in and the sub-device
     * announced. The message is acknowledged above whether or not anything acts on it, so the
     * pad considered the exchange finished and waited, as 2.2.11 tells an audio device to do.
     *
     * Last in the chain deliberately. Every comparison ahead of a message costs the per-packet
     * paths that run behind it - input at ~120 Hz and audio at 125 packets/s - and metadata
     * arrives twice per connect.
     */
    else if (
        frame->command == CMD_IDENTIFY &&
        payloadLength >= sizeof(IdentifyData) &&
        data.size() >= payloadLength
    ) {
        const IdentifyData *identify = data.toStruct<IdentifyData>();

        // Offsets inside are relative to the end of the opening blob, exactly as dispatchChunked()
        // resolves them, so the same two adjustments apply here.
        identifyReceived(frame->deviceId, identify,
                         data.raw() + sizeof(identify->unknown),
                         payloadLength - sizeof(identify->unknown));
    }

    // Ignore any unknown packets
    return true;
}

bool GipDevice::setDeviceState(uint8_t id, DeviceState state)
{
    Frame frame = {};

    frame.command = CMD_SET_DEVICE_STATE;
    frame.deviceId = id;
    frame.type = TYPE_REQUEST;
    frame.sequence = getSequence();
    frame.length = sizeof(uint8_t);

    Bytes out;

    out.append(frame);
    out.append(static_cast<uint8_t>(state));

    return sendPacket(out);
}

bool GipDevice::performRumble(RumbleData rumble)
{
    Frame frame = {};

    frame.command = CMD_RUMBLE;
    frame.type = TYPE_COMMAND;
    frame.sequence = getSequence();
    frame.length = sizeof(rumble);

    Bytes out;

    out.append(frame);
    out.append(rumble);

    return sendPacket(out);
}

bool GipDevice::setLedMode(LedModeData mode)
{
    Frame frame = {};

    frame.command = CMD_LED_MODE;
    frame.type = TYPE_REQUEST;
    frame.sequence = getSequence();
    frame.length = sizeof(mode);

    Bytes out;

    out.append(frame);
    out.append(mode);

    return sendPacket(out);
}

bool GipDevice::requestSerialNumber()
{
    Frame frame = {};

    frame.command = CMD_EXTENDED;
    frame.type = TYPE_REQUEST | TYPE_ACK;
    frame.sequence = getSequence();
    frame.length = sizeof(uint8_t);

    Bytes out;

    // The payload of an extended command is its sub-command byte (MS-GIPUSB 3.1.5.5.10). The
    // others are 0x00 Get Capabilities, which every device supporting 0x1e must implement, and
    // 0x02 Get Telemetry Data. Nothing here needs either.
    out.append(frame);
    out.append(static_cast<uint8_t>(EXT_CMD_SERIAL_NUMBER));

    return sendPacket(out);
}

/*
 * Accumulates one fragment of a fragmented message (MS-GIPUSB 3.1.5.2).
 *
 * The first fragment carries InitFrag and puts the *total* message length in the field where later
 * fragments put their offset - the spec calls it TLO for that reason. The transfer ends with an
 * empty fragment, at which point the reassembled message is dispatched.
 */
bool GipDevice::handleChunk(const Frame &frame, uint32_t length, uint32_t offset,
                            const Bytes &data)
{
    // This device's own slot. Another device's transfer may be in flight in parallel and must not
    // be disturbed by this one - see the ChunkTransfer comment in gip.h.
    ChunkTransfer &transfer = chunkFor(frame.deviceId);

    if (frame.type & TYPE_CHUNK_START)
    {
        // On the first fragment the offset field is the total length of what is coming
        if (offset > CHUNK_BUFFER_MAX_LENGTH)
        {
            Log::error("Chunked message too large: %u bytes", offset);

            return true;
        }

        if (transfer.active)
        {
            Log::debug("Discarding incomplete chunked message for device %u",
                       (unsigned)frame.deviceId);
        }

        transfer.buffer.assign(offset, 0);
        transfer.length = offset;
        transfer.command = frame.command;
        transfer.active = true;

        // The first fragment's own payload starts at zero
        offset = 0;
    }

    if (!transfer.active)
    {
        // A stray completion for a transfer we never saw the start of. Older controllers send
        // these spontaneously, so it is not worth logging as an error.
        return true;
    }

    // Still only catches a *different message type* on the same device - two transfers of the same
    // type now go to different slots rather than needing to be told apart here
    if (frame.command != transfer.command)
    {
        Log::error("Conflicting chunked message for device %u", (unsigned)frame.deviceId);

        transfer.active = false;
        transfer.buffer.clear();

        return true;
    }

    uint32_t received = offset + length;

    if (received > transfer.length || received < offset)
    {
        Log::error("Chunk overruns its message");

        transfer.active = false;
        transfer.buffer.clear();

        return true;
    }

    if (length > 0)
    {
        // Acknowledge when asked, and always on the final fragment: the sender waits for that one
        // before considering the transfer done, whether or not it set the flag.
        bool last = (received == transfer.length);

        if ((frame.type & TYPE_ACK) || last)
        {
            if (!acknowledgeChunk(frame, received, transfer.length - received))
            {
                Log::error("Failed to acknowledge chunk");

                return false;
            }
        }

        if (data.size() < length)
        {
            return true;
        }

        std::copy(data.begin(), data.begin() + length, transfer.buffer.begin() + offset);

        return true;
    }

    // An empty fragment ends the transfer
    dispatchChunked(frame.deviceId, transfer.command, transfer.buffer.data(), transfer.length);

    transfer.active = false;
    transfer.buffer.clear();

    return true;
}

/*
 * Handles a message that arrived fragmented. Only metadata is acted on; anything else is noted
 * and dropped, which is the same as before this reassembly existed, only now visibly.
 */
void GipDevice::dispatchChunked(uint8_t deviceId, uint8_t command, const uint8_t *data,
                                size_t length)
{
    if (command == CMD_IDENTIFY && length >= sizeof(IdentifyData))
    {
        const IdentifyData *identify = reinterpret_cast<const IdentifyData *>(data);

        // Offsets are relative to the end of the opening blob, not the start of the message
        identifyReceived(deviceId, identify, data + sizeof(identify->unknown),
                         length - sizeof(identify->unknown));

        return;
    }

    if (command == CMD_AUTHENTICATE)
    {
        handleAuthPacket(data, length);

        return;
    }

    Log::debug("Ignoring chunked message: command 0x%02x, %zu bytes", command, length);
}

/*
 * Acknowledges a fragment, reporting how much of the message has arrived contiguously and how much
 * is still outstanding, so the sender knows whether to retransmit (MS-GIPUSB 3.1.5.1).
 *
 * Distinct from acknowledgePacket() below, which answers a single unfragmented message and has no
 * progress to report.
 */
bool GipDevice::acknowledgeChunk(const Frame &frame, uint32_t received, uint32_t remaining)
{
    Frame header = {};

    header.command = CMD_ACKNOWLEDGE;
    header.deviceId = frame.deviceId;
    header.type = TYPE_REQUEST;
    header.sequence = frame.sequence;
    header.length = sizeof(AcknowledgeData);

    AcknowledgeData ack = {};

    ack.command = frame.command;
    ack.options = frame.deviceId | (TYPE_REQUEST << 4);
    ack.received = static_cast<uint16_t>(received);
    ack.remaining = static_cast<uint16_t>(remaining);

    Bytes out;

    out.append(header);
    out.append(ack);

    return sendPacket(out);
}

/*
 * Big-endian, because the auth headers are - the rest of GIP is little-endian, so this is written
 * out rather than reusing anything.
 */
static void putBigEndian16(uint8_t *buf, uint16_t value)
{
    buf[0] = static_cast<uint8_t>(value >> 8);
    buf[1] = static_cast<uint8_t>(value);
}

/*
 * Sends one handshake message and folds it into the transcript.
 *
 * Layout is the capture's: handshake header, data header, payload, trailer. Both lengths are
 * big-endian and each counts the bytes after its own header, excluding the trailer.
 *
 * The transcript covers everything from the data header onward - not the handshake header, not the
 * trailer - for our messages and the device's alike. Both sides hash it and compare in the finish
 * messages, so what goes in and what stays out is exact rather than a matter of taste.
 */
/*
 * Wraps a handshake message in its GIP header and sends it. Split out because both senders need
 * the extended length encoding: a message carrying the encrypted secret is 274 bytes.
 */
bool GipDevice::sendAuthFrame(const uint8_t *payload, size_t length)
{
    if (++securitySequence == 0x00)
    {
        securitySequence = 0x01;
    }

    uint8_t sequence = securitySequence;
    uint8_t header[HEADER_MAX_LENGTH];

    // Small enough to go whole. The hello and every request are well inside this.
    if (length <= AUTH_FRAGMENT_LENGTH)
    {
        size_t headerLength = encodeHeader(header, CMD_AUTHENTICATE, 0, TYPE_REQUEST | TYPE_ACK,
                                           sequence, static_cast<uint32_t>(length));

        Bytes out(headerLength + length);

        std::copy(header, header + headerLength, out.raw());
        std::copy(payload, payload + length, out.raw() + headerLength);

        return sendPacket(out);
    }

    /*
     * Anything larger is fragmented, which is how a captured host sends the 274-byte message that
     * carries the encrypted secret - and the mirror of how the device's certificate arrives here.
     * Sending it whole with an extended length instead put the pad into a connect loop: the link
     * carries AUTH_FRAGMENT_LENGTH bytes per frame, and this is not a matter of how the length is
     * written.
     *
     * Every fragment of one message shares its sequence number. Acknowledgement is asked for on the
     * first and last only, matching the capture.
     */
    for (size_t offset = 0; offset < length; offset += AUTH_FRAGMENT_LENGTH)
    {
        size_t remaining = length - offset;
        size_t chunk = remaining < AUTH_FRAGMENT_LENGTH ? remaining : AUTH_FRAGMENT_LENGTH;

        bool first = offset == 0;
        bool last = offset + chunk >= length;

        uint8_t type = TYPE_REQUEST | TYPE_CHUNK;

        if (first)
        {
            type |= TYPE_CHUNK_START;
        }

        if (first || last)
        {
            type |= TYPE_ACK;
        }

        // The first fragment carries the whole message's length where the others carry an offset
        size_t headerLength = encodeChunkHeader(header, CMD_AUTHENTICATE, 0, type, sequence,
                                                static_cast<uint32_t>(chunk),
                                                static_cast<uint32_t>(first ? length : offset));

        Bytes out(headerLength + chunk);

        std::copy(header, header + headerLength, out.raw());
        std::copy(payload + offset, payload + offset + chunk, out.raw() + headerLength);

        if (!sendPacket(out))
        {
            return false;
        }
    }

    // An empty fragment ends the transfer, carrying the total where an offset would go
    size_t headerLength = encodeChunkHeader(header, CMD_AUTHENTICATE, 0,
                                            TYPE_REQUEST | TYPE_CHUNK, sequence, 0,
                                            static_cast<uint32_t>(length));

    Bytes out(headerLength);

    std::copy(header, header + headerLength, out.raw());

    return sendPacket(out);
}

bool GipDevice::sendAuthPacket(uint8_t command, const uint8_t *payload, size_t length)
{
    const size_t dataLength = sizeof(AuthDataHeader) + length;
    const size_t total = sizeof(AuthHandshakeHeader) + dataLength + AUTH_TRAILER_LENGTH;

    Bytes packet(total);
    uint8_t *raw = packet.raw();

    std::fill(raw, raw + total, 0);

    raw[0] = 0x00;
    raw[1] = AUTH_OPT_ACKNOWLEDGE | AUTH_OPT_FROM_HOST;
    raw[2] = 0x00;
    raw[3] = command;
    putBigEndian16(raw + 4, static_cast<uint16_t>(dataLength));

    raw[6] = command;
    // Security protocol major version, which the command itself determines: the v2 command set
    // starts at AUTH2_HOST_HELLO.
    raw[7] = command >= AUTH2_HOST_HELLO ? 0x02 : 0x01;
    putBigEndian16(raw + 8, static_cast<uint16_t>(length));

    if (length > 0)
    {
        std::copy(payload, payload + length, raw + sizeof(AuthHandshakeHeader) + sizeof(AuthDataHeader));
    }

    authTranscript.insert(authTranscript.end(),
                          raw + sizeof(AuthHandshakeHeader),
                          raw + sizeof(AuthHandshakeHeader) + dataLength);

    authLastSent = command;

    return sendAuthFrame(packet.raw(), total);
}

bool GipDevice::sendAuthHostHello()
{
    // Always opens as v1; a v2 device upgrades from here. Reset so a reconnecting pad is not still
    // treated as v2 from its previous session.
    authVersion2 = false;

    authTranscript.clear();
    authPublicKeyClient2.clear();
    authMasterSecret.clear();
    authPublicKey.clear();
    authRandomClient.clear();

    // 32 random bytes, then two unknown four-byte fields the capture leaves zero
    authRandomHost = GipCrypto::randomBytes(AUTH_RANDOM_LENGTH);

    if (authRandomHost.size() != AUTH_RANDOM_LENGTH)
    {
        Log::error("Security: no random source");

        return false;
    }

    std::vector<uint8_t> payload(AUTH_RANDOM_LENGTH + 8, 0);

    std::copy(authRandomHost.begin(), authRandomHost.end(), payload.begin());

    return sendAuthPacket(AUTH_HOST_HELLO, payload.data(), payload.size());
}

bool GipDevice::requestAuthPacket(uint8_t command, uint16_t length)
{
    // A request carries the handshake header and the trailer, and no data header: the length says
    // how much the device should send back. It is not part of the transcript - nothing of ours
    // that the device does not hash can be.
    const size_t total = sizeof(AuthHandshakeHeader) + AUTH_TRAILER_LENGTH;

    Bytes payload(total);
    uint8_t *raw = payload.raw();

    std::fill(raw, raw + total, 0);

    raw[0] = 0x00;
    raw[1] = AUTH_OPT_REQUEST | AUTH_OPT_FROM_HOST;
    raw[2] = 0x00;
    raw[3] = command;
    putBigEndian16(raw + 4, length);

    authLastSent = command;

    return sendAuthFrame(payload.raw(), total);
}

/*
 * TLS-style P_hash over HMAC-SHA256: A(1) = HMAC(key, label || seed), then each output block is
 * HMAC(key, A(i) || label || seed) with A(i+1) = HMAC(key, A(i)).
 */
std::vector<uint8_t> GipDevice::computePrf(const char *label,
                                           const std::vector<uint8_t> &key,
                                           const std::vector<uint8_t> &seed,
                                           size_t length)
{
    std::vector<uint8_t> labelAndSeed(label, label + strlen(label));

    labelAndSeed.insert(labelAndSeed.end(), seed.begin(), seed.end());

    std::vector<uint8_t> a = GipCrypto::hmacSha256(key.data(), key.size(),
                                                   labelAndSeed.data(), labelAndSeed.size());
    std::vector<uint8_t> out;

    while (out.size() < length && !a.empty())
    {
        std::vector<uint8_t> block(a);

        block.insert(block.end(), labelAndSeed.begin(), labelAndSeed.end());

        std::vector<uint8_t> digest = GipCrypto::hmacSha256(key.data(), key.size(),
                                                            block.data(), block.size());

        if (digest.empty())
        {
            return {};
        }

        out.insert(out.end(), digest.begin(), digest.end());
        a = GipCrypto::hmacSha256(key.data(), key.size(), a.data(), a.size());
    }

    if (out.size() < length)
    {
        return {};
    }

    out.resize(length);

    return out;
}


/*
 * Lifts the controller's RSA public key out of its certificate.
 *
 * Scanned for rather than parsed, because the certificate cannot be parsed: the ones Microsoft
 * issues have an empty subject and no subjectAltName, which RFC 5280 section 4.2.1.6 forbids, and a
 * conforming X.509 parser rejects them. Nothing is verified - there is no trust decision to make
 * here, only a key to encrypt a secret with, and the device proves it holds the private half by
 * producing a finish value we can predict.
 *
 * The pattern is the DER header of a 2048-bit RSAPublicKey.
 */
bool GipDevice::extractPublicKey(const uint8_t *data, size_t length)
{
    static const uint8_t marker[] = { 0x30, 0x82, 0x01, 0x0a };

    for (size_t i = 0; i + sizeof(marker) <= length; i++)
    {
        if (memcmp(data + i, marker, sizeof(marker)) != 0)
        {
            continue;
        }

        if (i + AUTH_PUBKEY_LENGTH > length)
        {
            return false;
        }

        authPublicKey.assign(data + i, data + i + AUTH_PUBKEY_LENGTH);

        return true;
    }

    return false;
}

/*
 * Encrypts a fresh pre-master secret under the device's key and sends it.
 *
 * The master secret is derived on both sides from that secret and the two randoms, so from here on
 * the device can be checked rather than trusted: only something holding the certificate's private
 * half can recover the secret and produce the finish value we compute below.
 */
bool GipDevice::sendAuthHostSecret()
{
    std::vector<uint8_t> secret = GipCrypto::randomBytes(AUTH_SECRET_LENGTH);

    if (secret.size() != AUTH_SECRET_LENGTH)
    {
        return false;
    }

    std::vector<uint8_t> randoms(authRandomHost);

    randoms.insert(randoms.end(), authRandomClient.begin(), authRandomClient.end());

    authMasterSecret = computePrf("Master Secret", secret, randoms, AUTH_SECRET_LENGTH);

    if (authMasterSecret.empty())
    {
        Log::error("Security: failed to derive the master secret");

        return false;
    }

    std::vector<uint8_t> encrypted = GipCrypto::rsaEncrypt(authPublicKey.data(),
                                                            authPublicKey.size(),
                                                            secret.data(), secret.size());

    if (encrypted.empty())
    {
        Log::error("Security: failed to encrypt the pre-master secret");

        return false;
    }

    return sendAuthPacket(AUTH_HOST_SECRET, encrypted.data(), encrypted.size());
}

/*
 * Restarts the exchange as version 2, after a device declined version 1.
 *
 * Everything from the first attempt is discarded, the transcript above all: it is hashed into both
 * finish messages, so carrying a v1 message into a v2 transcript would make the two sides disagree
 * about what was said and fail the exchange at the very last step.
 */
bool GipDevice::sendAuthHostHello2()
{
    authVersion2 = true;

    authTranscript.clear();
    authMasterSecret.clear();
    authPublicKey.clear();
    authPublicKeyClient2.clear();
    authRandomClient.clear();

    // A fresh random too - the first one belongs to an exchange that is being abandoned
    authRandomHost = GipCrypto::randomBytes(AUTH_RANDOM_LENGTH);

    if (authRandomHost.size() != AUTH_RANDOM_LENGTH)
    {
        Log::error("Security: no random source");

        return false;
    }

    // 32 random bytes then four the capture leaves zero, where v1 has eight
    std::vector<uint8_t> payload(AUTH_RANDOM_LENGTH + 4, 0);

    std::copy(authRandomHost.begin(), authRandomHost.end(), payload.begin());

    return sendAuthPacket(AUTH2_HOST_HELLO, payload.data(), payload.size());
}

/*
 * Version 2's replacement for the RSA-encrypted pre-master secret: agree a secret by ECDH and send
 * our public key so the device can agree the same one.
 *
 * The certificate is not involved. v1 encrypts to the key inside it, which at least ties the secret
 * to that certificate; v2 never uses it, so the exchange is unauthenticated either way - there was
 * no trust decision in v1 either, since the certificate is never validated.
 */
bool GipDevice::sendAuthHostPubkey()
{
    // Our public key and the agreed secret in one call; the private half is never needed again
    std::vector<uint8_t> result = GipCrypto::ecdhP256(authPublicKeyClient2.data(),
                                                      authPublicKeyClient2.size());

    if (result.size() != AUTH2_PUBKEY_LENGTH + AUTH2_SECRET_LENGTH)
    {
        Log::error("Security: ECDH failed");

        return false;
    }

    std::vector<uint8_t> randoms(authRandomHost);

    randoms.insert(randoms.end(), authRandomClient.begin(), authRandomClient.end());

    authMasterSecret = computePrf("Master Secret",
                                  std::vector<uint8_t>(result.begin() + AUTH2_PUBKEY_LENGTH,
                                                       result.end()),
                                  randoms, AUTH_SECRET_LENGTH);

    if (authMasterSecret.empty())
    {
        Log::error("Security: failed to derive the master secret");

        return false;
    }

    return sendAuthPacket(AUTH2_HOST_PUBKEY, result.data(), AUTH2_PUBKEY_LENGTH);
}

/*
 * Proves we derived the same master secret, by sending a value derived from it and from every
 * handshake message so far. The device checks it and answers with its own.
 */
bool GipDevice::sendAuthFinish()
{
    std::vector<uint8_t> transcript = GipCrypto::sha256(authTranscript.data(),
                                                         authTranscript.size());

    if (transcript.empty())
    {
        return false;
    }

    std::vector<uint8_t> finish = computePrf("Host Finished", authMasterSecret, transcript,
                                              transcript.size());

    if (finish.empty())
    {
        return false;
    }

    return sendAuthPacket(authVersion2 ? AUTH2_HOST_FINISH : AUTH_HOST_FINISH,
                          finish.data(), finish.size());
}

/*
 * The device acknowledged what we last sent, so ask for whatever it owes us next. The exchange is
 * host-driven throughout: a reply is requested, never merely awaited. Sizes are the capture's.
 */
void GipDevice::handleAuthAcknowledge()
{
    switch (authLastSent)
    {
        case AUTH_HOST_HELLO:
            requestAuthPacket(AUTH_CLIENT_HELLO, 0x0054);
            break;

        case AUTH_HOST_SECRET:
            sendAuthFinish();
            break;

        case AUTH_HOST_FINISH:
            requestAuthPacket(AUTH_CLIENT_FINISH, 0x0044);
            break;

        /*
         * Version 2. Sizes are xone's packet structures: a client hello is 32 random plus 140
         * unknown, a certificate 768, a public key 64 plus 64 unknown, and a finish two 32-byte
         * halves. Unverified against hardware - see the header.
         */
        case AUTH2_HOST_HELLO:
            requestAuthPacket(AUTH2_CLIENT_HELLO, 172);
            break;

        case AUTH2_HOST_PUBKEY:
            sendAuthFinish();
            break;

        case AUTH2_HOST_FINISH:
            requestAuthPacket(AUTH2_CLIENT_FINISH, 64);
            break;

        default:
            break;
    }
}

void GipDevice::handleAuthPacket(const uint8_t *data, size_t length)
{
    if (length < sizeof(AuthHandshakeHeader))
    {
        return;
    }

    uint8_t options = data[1];
    uint8_t error = data[2];
    uint8_t command = data[3];

    Log::info("Security: command 0x%02x, error 0x%02x, %zu bytes: %02x %02x %02x %02x %02x %02x",
              command, error, length,
              data[0], data[1], data[2], data[3],
              length > 4 ? data[4] : 0, length > 5 ? data[5] : 0);

    if (error != 0)
    {
        Log::error("Security: device reported error 0x%02x", error);

        return;
    }

    // An acknowledgement, not content: command 0x01 means the last message was accepted, anything
    // else means it was rejected and the exchange is over.
    if (options & AUTH_OPT_ACKNOWLEDGE)
    {
        if (command != 0x01)
        {
            Log::error("Security: handshake rejected, 0x%02x", command);

            return;
        }

        handleAuthAcknowledge();

        return;
    }

    if (length < sizeof(AuthHandshakeHeader) + sizeof(AuthDataHeader))
    {
        return;
    }

    /*
     * The data header states the security protocol version outright, so take the device at its
     * word rather than inferring. xone instead notices the two headers disagreeing, which is a
     * consequence of the same thing - a v2 device answering a v1 hello - so both are checked and
     * either is enough.
     *
     * Version 2 negotiates ECDH in place of an RSA-encrypted secret, and is implemented below as
     * an upgrade: the exchange restarts from a fresh transcript rather than being translated
     * mid-flight. An Elite Series 2 (045e:0b00, firmware 5.23.6.0) takes this path and completes.
     */
    uint8_t version = data[7];

    /*
     * A device that wants version 2 says so twice over: the data header's version byte reads 0x02,
     * and its two command bytes disagree because the handshake header still echoes the v1 command
     * we sent. Either is enough, and both are checked - the version byte is the direct statement
     * and the mismatch is the consequence, which is all xone looks at.
     *
     * The response is to start again rather than to translate mid-exchange: the transcript is
     * hashed into the finish messages, so a v2 handshake has to hash a v2 transcript and nothing
     * exchanged so far may remain in it.
     */
    if (!authVersion2 && (version == 0x02 || data[3] != data[6]))
    {
        Log::info("Security: device wants protocol v2, restarting the exchange");

        sendAuthHostHello2();

        return;
    }

    if (version != (authVersion2 ? 0x02 : 0x01) || data[3] != data[6])
    {
        // v1 and v2 are both implemented, and the upgrade above has already run if it was going
        // to, so anything reaching here is a version this driver has never seen - or a header
        // pair that disagrees for some reason other than the upgrade.
        Log::error("Security: device wants protocol v%u, which is not implemented", version);

        authLastSent = 0;

        return;
    }

    const uint8_t *payload = data + sizeof(AuthHandshakeHeader) + sizeof(AuthDataHeader);
    size_t payloadLength = length - sizeof(AuthHandshakeHeader) - sizeof(AuthDataHeader);

    switch (command)
    {
        case AUTH_CLIENT_HELLO:
        case AUTH2_CLIENT_HELLO:
            if (payloadLength >= AUTH_RANDOM_LENGTH)
            {
                authRandomClient.assign(payload, payload + AUTH_RANDOM_LENGTH);
            }
            break;

        // Version 2 asks for the certificate and hashes it into the transcript, but takes no key
        // from it - the key material is the ECDH exchange two messages later.
        case AUTH2_CLIENT_PUBKEY:
            if (payloadLength >= AUTH2_PUBKEY_LENGTH)
            {
                authPublicKeyClient2.assign(payload, payload + AUTH2_PUBKEY_LENGTH);
            }
            break;

        case AUTH_CLIENT_CERTIFICATE:
            if (!extractPublicKey(payload, payloadLength))
            {
                Log::error("Security: no public key in the certificate");
            }
            break;

        default:
            break;
    }

    // Everything from the data header onward, for the device's messages as for ours
    authTranscript.insert(authTranscript.end(),
                          data + sizeof(AuthHandshakeHeader),
                          data + length);

    switch (command)
    {
        case AUTH_CLIENT_HELLO:
            requestAuthPacket(AUTH_CLIENT_CERTIFICATE, 0x0404);
            break;

        case AUTH_CLIENT_CERTIFICATE:
            if (!authPublicKey.empty())
            {
                sendAuthHostSecret();
            }
            break;

        case AUTH2_CLIENT_HELLO:
            requestAuthPacket(AUTH2_CLIENT_CERTIFICATE, 768);
            break;

        case AUTH2_CLIENT_CERTIFICATE:
            requestAuthPacket(AUTH2_CLIENT_PUBKEY, 128);
            break;

        case AUTH2_CLIENT_PUBKEY:
            if (!authPublicKeyClient2.empty())
            {
                sendAuthHostPubkey();
            }
            else
            {
                Log::error("Security: device sent no usable public key");
            }
            break;

        case AUTH2_CLIENT_FINISH:
        case AUTH_CLIENT_FINISH:
        {
            /*
             * The exchange is closed by a two-byte message in the control context rather than the
             * handshake one: context 0x01, control 0x00 for complete. A captured host sends it
             * immediately after the device's finish, and without it the device is never told the
             * handshake succeeded.
             */
            static const uint8_t complete[] = { 0x01, 0x00 };

            sendAuthFrame(complete, sizeof(complete));

            Log::info("Security: handshake complete");

            // The session key is what the link encryption needs; a transport without any ignores it
            std::vector<uint8_t> key = computePrf("Session Key", authMasterSecret,
                                                   authRandomHost, 16);

            if (!key.empty())
            {
                authCompleted(key.data(), key.size());
            }

            authLastSent = 0;
            break;
        }

        default:
            break;
    }
}

bool GipDevice::setAudioFormat(uint8_t id, AudioFormat in, AudioFormat out)
{
    Frame frame = {};

    frame.deviceId = id;
    frame.command = CMD_AUDIO_CONFIG;
    frame.type = TYPE_REQUEST;
    frame.sequence = getSequence();
    frame.length = sizeof(AudioFormatData);

    AudioFormatData format = {};

    format.subcommand = AUDIO_CTRL_FORMAT;
    format.in = in;
    format.out = out;

    Bytes packet;

    packet.append(frame);
    packet.append(format);

    return sendPacket(packet);
}

bool GipDevice::setAudioVolume(uint8_t id, const AudioVolumeData &volume)
{
    Frame frame = {};

    frame.deviceId = id;
    frame.command = CMD_AUDIO_CONFIG;
    frame.type = TYPE_REQUEST;
    frame.sequence = getSequence();
    frame.length = sizeof(AudioVolumeData);

    Bytes packet;

    packet.append(frame);
    packet.append(volume);

    return sendPacket(packet);
}

size_t GipDevice::encodeAudioMessage(uint8_t id, const uint8_t *samples, size_t length,
                                     uint8_t *out)
{
    // Audio needs the extended length encoding - 1536 bytes will not fit the single byte the
    // Frame struct has - so the header is built by hand rather than through Frame.
    //
    // Zero is reserved, so skip it on wrap
    if (++audioSequence == 0)
    {
        audioSequence = 1;
    }

    // Addressed to the audio sub-device, which is where the audio lives - not to the pad
    size_t headerLength = encodeHeader(out, CMD_AUDIO_SAMPLES, id, TYPE_REQUEST,
                                       audioSequence, static_cast<uint32_t>(length));

    std::copy(samples, samples + length, out + headerLength);

    return headerLength + length;
}

bool GipDevice::sendAudioSamples(uint8_t id, const uint8_t *samples, size_t length)
{
    // Sized once and filled directly. Bytes::append()'s template overload takes the address of
    // whatever it is handed, which for a pointer would copy the pointer rather than the samples.
    Bytes packet(HEADER_MAX_LENGTH + length);

    size_t used = encodeAudioMessage(id, samples, length, packet.raw());

    // Trimmed to what the header actually needed, which is usually shorter than the maximum
    Bytes trimmed(packet, 0, packet.size() - used);

    return sendPacket(trimmed);
}

bool GipDevice::requestIdentify(uint8_t id)
{
    Frame frame = {};

    frame.deviceId = id;
    frame.command = CMD_IDENTIFY;
    frame.type = TYPE_REQUEST;
    frame.sequence = getSequence();
    frame.length = 0;

    Bytes out;

    out.append(frame);

    return sendPacket(out);
}

bool GipDevice::acknowledgePacket(Frame frame)
{
    Frame header = {};

    header.command = CMD_ACKNOWLEDGE;
    header.deviceId = frame.deviceId;
    header.type = TYPE_REQUEST;
    header.sequence = frame.sequence;
    header.length = sizeof(header) + 5;

    frame.type = TYPE_REQUEST;
    frame.sequence = frame.length;
    frame.length = 0;

    Bytes out;

    out.append(header);
    out.pad(1);
    out.append(frame);
    out.pad(4);

    return sendPacket(out);
}

uint8_t GipDevice::getSequence(bool accessory)
{
    if (accessory)
    {
        // Zero is an invalid sequence number
        if (accessorySequence == 0x00)
        {
            accessorySequence = 0x01;
        }

        return accessorySequence++;
    }

    if (sequence == 0x00)
    {
        sequence = 0x01;
    }

    return sequence++;
}

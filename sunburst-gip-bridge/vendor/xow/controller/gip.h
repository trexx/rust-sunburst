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

#pragma once

#include <array>
#include <cstddef>
#include <cstdint>
#include <functional>
#include <vector>

struct Frame;
class Bytes;

/*
 * Base class for GIP (Game Input Protocol) devices.
 *
 * The handshake, and the state it drives the device through (MS-GIPUSB 3.1.1):
 *
 *   <- Announce             (from controller)          device is in Arrival
 *   -> Identify             (metadata request)         Arrival -> Idle
 *   <- Identify             (metadata response)
 *   -> Set device state     (start)                    Idle -> Active
 *   -> LED mode: the configured guide button brightness
 *   -> Extended command     (get serial number)
 *   <- Extended command     (serial number response)
 *
 * Start comes after the metadata response rather than alongside the request: a device leaves the
 * Hello stage only on receipt of a state message, and an audio sub-device announces itself only
 * once the primary has initialised (2.2.11). A device that never answers the metadata request is
 * started anyway after a timeout - see Controller::waitForMetadata().
 *
 * The security exchange (command 6) runs at the end of startDevice(), after the metadata response.
 * It is not a security feature here - the certificate is never validated and the link is left
 * unencrypted - but 2.2.1.4 makes it the gate on sub-device enumeration, so without it a pad never
 * announces its audio device and headphone audio is unreachable. See sendAuthHostHello().
 *
 * This comment used to say the exchange was not implemented "because MS-GIPUSB 5 says the host
 * succeeds it by default". That is a misreading serious enough to be worth recording: section 5
 * describes the *host* accepting the exchange by default, not the exchange being skippable. It
 * cost this driver a working audio path until a USB capture of a Windows host showed the
 * handshake happening in full.
 */
class GipDevice
{
public:
    using SendPacket = std::function<bool(const Bytes &data)>;

    bool handlePacket(const Bytes &packet);

protected:
    enum BatteryType
    {
        BATT_TYPE_CHARGING = 0x00,
        BATT_TYPE_ALKALINE = 0x01,
        BATT_TYPE_NIMH = 0x02,
    };

    enum BatteryLevel
    {
        BATT_LEVEL_EMPTY = 0x00,
        BATT_LEVEL_LOW = 0x01,
        BATT_LEVEL_MED = 0x02,
        BATT_LEVEL_FULL = 0x03,
    };

    /*
     * States for the Set Device State command (MS-GIPUSB 3.1.5.5.5, Table 39). xow called this
     * "power mode" and named 0x01 SLEEP, which is wrong twice over: the message drives the device
     * state machine in 3.1.1 rather than a power rail, and 0x01 is Stop - the state the spec has
     * the host send to an *audio* device where a non-audio device gets Start.
     *
     * The device leaves the Hello stage only on receipt of one of these, so when it is sent is as
     * load-bearing as what it says. 0x03 (Full power) and 0x06 (Reserved) are omitted; nothing
     * here sends them.
     */
    enum DeviceState
    {
        STATE_START = 0x00,
        STATE_STOP = 0x01,
        STATE_OFF = 0x04,
        // Full teardown and reinitialise. The device must reply with a "powering off" status first
        STATE_RESET = 0x07,
    };

    enum LedMode
    {
        LED_OFF = 0x00,
        LED_ON = 0x01,
        LED_BLINK_FAST = 0x02,
        LED_BLINK_MED = 0x03,
        LED_BLINK_SLOW = 0x04,
        LED_FADE_SLOW = 0x08,
        LED_FADE_FAST = 0x09,
    };

    struct AnnounceData
    {
        uint8_t macAddress[6];
        uint16_t unknown;
        uint16_t vendorId;
        uint16_t productId;

        struct
        {
            uint16_t major;
            uint16_t minor;
            uint16_t build;
            uint16_t revision;
        } __attribute__((packed)) firmwareVersion;

        /*
         * Four separate versions, one byte per part (MS-GIPUSB Table 27, offsets 24 to 31). xow
         * read all eight bytes as a second four-part 16-bit version and called the lot
         * "hardwareVersion", which is why a pad reporting hardware 2.1 logged as "258.1.1.1":
         * 0x0102 is the major and minor bytes read as one little-endian word.
         *
         * The last three are protocol versions the specification pins: RF and GIP MUST both be
         * 1.0, and so MUST security. The security one is the useful one - it says which handshake
         * a pad expects before we have to infer it from what it answers.
         */
        struct
        {
            uint8_t major;
            uint8_t minor;
        } __attribute__((packed)) hardwareVersion, rfVersion, securityVersion, gipVersion;
    } __attribute__((packed));

    struct StatusData
    {
        uint32_t batteryLevel : 2;
        uint32_t batteryType : 2;
        uint32_t connectionInfo : 4;
        uint8_t unknown1;
        uint16_t unknown2;
    } __attribute__((packed));

    struct GuideButtonData
    {
        uint8_t pressed;
        uint8_t unknown;
    } __attribute__((packed));

    struct RumbleData
    {
        uint8_t unknown;
        uint8_t setRight : 1;
        uint8_t setLeft : 1;
        uint8_t setRightTrigger : 1;
        uint8_t setLeftTrigger : 1;
        uint8_t padding : 4;
        uint8_t leftTrigger;
        uint8_t rightTrigger;
        uint8_t left;
        uint8_t right;
        uint8_t duration;
        uint8_t delay;
        uint8_t repeat;
    } __attribute__((packed));

    struct LedModeData
    {
        uint8_t unknown;
        uint8_t mode;
        uint8_t brightness;
    } __attribute__((packed));

    /*
     * Sub-commands of the Extended Command message, 0x1e (MS-GIPUSB 3.1.5.5.10). The message type
     * is an envelope rather than a single command - xow named it CMD_SERIAL_NUM after the only one
     * of these it uses.
     */
    enum ExtendedCommand
    {
        EXT_CMD_CAPABILITIES = 0x00,
        EXT_CMD_TELEMETRY = 0x02,
        EXT_CMD_SERIAL_NUMBER = 0x04,
    };

    /*
     * Response to Get Serial Number (MS-GIPUSB 3.1.5.5.10.2). Every extended response opens with
     * its sub-command byte and a status byte - which is what xow read as one 16-bit "unknown" -
     * and the fields after status are present only when status is 0, EXT_CMD_STATUS_OK.
     */
    struct SerialData
    {
        uint8_t subcommand;
        uint8_t status;
        char serialNumber[14];
    } __attribute__((packed));

    /*
     * Metadata response header (MS-GIPUSB 2.2.2.4, "Device Metadata Object"), the reply to a
     * metadata request. Every field after the opening blob is a byte offset into the payload
     * that follows it - zero meaning "this device has none" - and each of those points at a
     * count byte followed by that many fixed-size items.
     *
     * Offsets are relative to the end of 'unknown', not to the start of the message.
     */
    struct IdentifyData
    {
        uint8_t unknown[16];
        uint16_t clientCommandsOffset;
        uint16_t firmwareVersionsOffset;
        uint16_t audioFormatsOffset;
        uint16_t capabilitiesOutOffset;
        uint16_t capabilitiesInOffset;
        uint16_t classesOffset;
        uint16_t interfacesOffset;
        uint16_t hidDescriptorOffset;
    } __attribute__((packed));

    /*
     * Audio format identifiers (MS-GIPUSB 2.2.2.4.3). Even values are stereo and odd are mono, so
     * channel count is 2 - (format & 1). Only the one this driver uses is named; the rest of the
     * table runs 8 kHz to 48 kHz in the same pairs.
     */
    enum AudioFormat
    {
        AUDIO_FORMAT_NONE = 0x00,
        AUDIO_FORMAT_48KHZ_MONO = 0x0f,
        AUDIO_FORMAT_48KHZ_STEREO = 0x10,
    };

    // Subcommand byte of an audio control message
    enum AudioControl
    {
        AUDIO_CTRL_FORMAT = 0x02,
        // Sent by the device, not the host: 2.2.11 has the host wait for it before playing audio
        AUDIO_CTRL_VOLUME = 0x03,
    };

    struct AudioFormatData
    {
        uint8_t subcommand;
        uint8_t in;
        uint8_t out;
    } __attribute__((packed));

    /*
     * Audio Control: Volume Extended (MS-GIPUSB 3.2.5.1.1, Table 66). The same eight bytes travel
     * in both directions: the device sends it unprompted to report its levels, and the host sends
     * it to ask for new ones.
     *
     * Bit 7 of each level field is not part of the level - it is the device declaring whether the
     * host may write that field. 3.2.5.1.1 is explicit that "the host SHOULD never issue a volume
     * request unless the device flags at least one volume field as writeable", so the flags the
     * device reported are preserved and echoed back rather than invented, and a request is only
     * sent for a field the device said was writable.
     *
     * The plain (non-extended) Volume message is referenced by 3.1.5 but has no format table in
     * the specification. Devices are told apart by payload length rather than guessing at it.
     */
    struct AudioVolumeData
    {
        uint8_t subcommand;
        uint8_t flags;
        uint8_t speaker;
        uint8_t balance;
        uint8_t microphone;
        uint8_t sidetone;
        uint8_t reserved[2];
    } __attribute__((packed));

    // Bit 7 of a volume field is the writable flag; the level is the low seven bits, 0 - 100.
    static const uint8_t AUDIO_VOLUME_WRITABLE = 0x80;
    static const uint8_t AUDIO_VOLUME_LEVEL = 0x7f;

    /*
     * Upstream audio, which the pad sends whether or not it has a microphone. The flow rate is the
     * point of it here: it is how GIP absorbs clock drift, and a value that wanders away from the
     * configured buffer size means our audio is slipping against the device's clock.
     */
    struct AudioSamplesData
    {
        uint16_t flowRate;
    } __attribute__((packed));

    struct InputData
    {
        struct
        {
            uint32_t unknown : 2;
            uint32_t start : 1;
            uint32_t select : 1;
            uint32_t a : 1;
            uint32_t b : 1;
            uint32_t x : 1;
            uint32_t y : 1;
            uint32_t dpadUp : 1;
            uint32_t dpadDown : 1;
            uint32_t dpadLeft : 1;
            uint32_t dpadRight : 1;
            uint32_t bumperLeft : 1;
            uint32_t bumperRight : 1;
            uint32_t stickLeft : 1;
            uint32_t stickRight : 1;
        } __attribute__((packed)) buttons;

        uint16_t triggerLeft;
        uint16_t triggerRight;
        int16_t stickLeftX;
        int16_t stickLeftY;
        int16_t stickRightX;
        int16_t stickRightY;
    } __attribute__((packed));

    GipDevice(SendPacket sendPacket);
    virtual ~GipDevice() = default;

    virtual void deviceAnnounced(uint8_t id, const AnnounceData *announce) = 0;
    virtual void statusReceived(uint8_t id, const StatusData *status) = 0;
    /*
     * The device id is passed to every handler, including the ones where only the primary device
     * is expected to use them.
     *
     * Dropping it was a bug three times over. A sub-device's Status was applied to the pad's
     * battery until it was routed properly, and these four were the same mistake still standing:
     * whatever a sub-device sends in these classes would be taken as the pad's own. Input is the
     * one that matters - a chatpad is a sub-device that sends input reports, and they would have
     * been parsed as the pad's sticks and buttons.
     *
     * Deciding what to do with a sub-device's message belongs to the subclass, not here. This
     * layer speaks GIP, where a sub-device sending input is meaningful; it is only meaningless to
     * a class that models a single pad.
     */
    virtual void guideButtonPressed(uint8_t id, const GuideButtonData *button) = 0;
    virtual void serialNumberReceived(uint8_t id, const SerialData *serial) = 0;
    virtual void inputReceived(uint8_t id, const InputData *input) = 0;

    /*
     * The reassembled metadata response. 'payload' points at the bytes the offsets in 'identify'
     * are relative to, and is valid only for the duration of the call.
     *
     * Not pure virtual: a device that never asks for metadata has no reason to implement it.
     */
    virtual void identifyReceived(uint8_t id, const IdentifyData *identify,
                                  const uint8_t *payload, size_t length) {}

    /*
     * Sets the device state (MS-GIPUSB 3.1.5.5.5). 'id' is the expansion index: 0 is the primary
     * device, and a secondary device such as an audio sub-device is addressed by its own index.
     */
    bool setDeviceState(uint8_t id, DeviceState state);
    /*
     * Security exchange (MS-GIPUSB 3.1.5.5, message type 0x06), the handshake a Windows host runs
     * against every pad. A capture of one shows it completing, and the pad's audio sub-device
     * announcing a few seconds later; this driver has never sent any of it and no sub-device has
     * ever appeared. See AUDIO.md.
     *
     * The framing follows xone: an outer handshake header, an inner data header, the payload, then
     * an eight-byte trailer. Lengths in both headers are **big-endian**, unlike everywhere else in
     * GIP, and each counts the bytes after the header it sits in.
     */
    struct AuthHandshakeHeader
    {
        uint8_t context;
        uint8_t options;
        uint8_t error;
        uint8_t command;
        uint16_t length;   // big-endian
    } __attribute__((packed));

    struct AuthDataHeader
    {
        uint8_t command;
        uint8_t version;
        uint16_t length;   // big-endian
    } __attribute__((packed));

    /*
     * Handshake commands. Version 1 exchanges an RSA-encrypted pre-master secret; version 2
     * replaces that one step with an ECDH exchange over P-256 and leaves everything else alone -
     * same framing, same transcript, same PRF, same finish.
     *
     * Which one is in use is not chosen up front. The host opens with a v1 hello and a v2 device
     * answers with its two header command bytes disagreeing, at which point the handshake restarts
     * as v2 from a fresh transcript. See handleAuthPacket().
     */
    enum AuthCommand
    {
        AUTH_HOST_HELLO = 0x01,
        AUTH_CLIENT_HELLO = 0x02,
        AUTH_CLIENT_CERTIFICATE = 0x03,
        AUTH_HOST_SECRET = 0x05,
        AUTH_HOST_FINISH = 0x07,
        AUTH_CLIENT_FINISH = 0x08,

        AUTH2_HOST_HELLO = 0x21,
        AUTH2_CLIENT_HELLO = 0x22,
        AUTH2_CLIENT_CERTIFICATE = 0x23,
        AUTH2_CLIENT_PUBKEY = 0x24,
        AUTH2_HOST_PUBKEY = 0x25,
        AUTH2_HOST_FINISH = 0x26,
        AUTH2_CLIENT_FINISH = 0x27,
    };

    enum AuthOption
    {
        AUTH_OPT_ACKNOWLEDGE = 0x01,
        AUTH_OPT_REQUEST = 0x02,
        AUTH_OPT_FROM_HOST = 0x40,
    };

    // Random bytes in a hello, and the fixed trailer every handshake message ends with
    static const size_t AUTH_RANDOM_LENGTH = 32;
    static const size_t AUTH_TRAILER_LENGTH = 8;

    /*
     * A 2048-bit DER RSAPublicKey: four header bytes plus the 0x010a the SEQUENCE declares. The
     * pre-master secret is 48 bytes and encrypts to the modulus size, 256.
     */
    static const size_t AUTH_PUBKEY_LENGTH = 270;
    static const size_t AUTH_SECRET_LENGTH = 48;

    /*
     * Version 2's key material. The public key is a raw affine point on NIST P-256 - 32 bytes of X
     * then 32 of Y, no uncompressed-point prefix - and the agreed secret is the X coordinate,
     * hashed with SHA-256 before the PRF sees it.
     */
    static const size_t AUTH2_PUBKEY_LENGTH = 64;
    static const size_t AUTH2_SECRET_LENGTH = 32;

    /*
     * Payload bytes per fragment of a handshake message.
     *
     * MS-GIPUSB 2.2.10.4 gives the Command data class an MTU of 64 bytes, and a fragment header is
     * six - command, flags, sequence, then the length and offset varints padded to an even total -
     * so 58 is what is left. A captured Windows host splits its 274-byte secret into four of these
     * and a 42-byte remainder, and xone's GIP_PKT_MAX_LENGTH is the same number.
     */
    static const size_t AUTH_FRAGMENT_LENGTH = 58;

    /* Opens the security exchange, as version 1. A v2 device upgrades it from there. */
    bool sendAuthHostHello();

    /*
     * Called once the handshake completes, with the 16-byte session key the exchange derives.
     * Not pure virtual: a transport with no link encryption has nothing to do with it. The
     * wireless adapter programs it into the radio; a cabled device does not.
     */
    virtual void authCompleted(const uint8_t *sessionKey, size_t length) {}

    /*
     * Asks the device for a handshake message it owes us. The exchange is host-driven: a reply is
     * requested rather than merely awaited.
     */
    bool requestAuthPacket(uint8_t command, uint16_t length);


    /*
     * Upstream audio from the pad (MS-GIPUSB 3.2.5.1.4): the flow rate for rate adaptation, then
     * the microphone PCM after it. `pcm`/`pcmLen` are the samples following the flow rate (empty
     * for a pad with no microphone, which still sends the flow rate). Sunburst addition: the
     * capture PCM was formerly discarded here.
     */
    virtual void audioSamplesReceived(uint8_t id, const AudioSamplesData *samples,
                                      const uint8_t *pcm, size_t pcmLen) {}

    /*
     * An Audio Control message from the device: the format it adopted, or its volume. Both are
     * steps the host waits on in 2.2.11's initialisation sequence.
     */
    virtual void audioControlReceived(uint8_t id, const uint8_t *data, size_t length) {}

    bool performRumble(RumbleData rumble);
    bool setLedMode(LedModeData mode);
    bool requestSerialNumber();
    bool requestIdentify(uint8_t id = 0);

    /* Asks the device to use these formats. 'in' is capture, 'out' is what we render to it. */
    bool setAudioFormat(uint8_t id, AudioFormat in, AudioFormat out);

    /* Asks the device for new volume levels. Caller supplies the flags the device itself reported. */
    bool setAudioVolume(uint8_t id, const AudioVolumeData &volume);

    /*
     * Sends one audio packet. 'length' must match the negotiated format's 8 ms buffer size, which
     * for 48 kHz stereo is 1536 bytes - the device paces on packet arrival, so a short packet is
     * heard rather than merely inefficient.
     */
    bool sendAudioSamples(uint8_t id, const uint8_t *samples, size_t length);

    /*
     * Writes one audio message - GIP header then samples - into a caller-supplied buffer, and
     * takes the next audio sequence number.
     *
     * Exists because the two transports packetise differently. The adapter sends one message per
     * 8 ms buffer; a cabled pad sends one per millisecond into an isochronous packet, so it needs
     * to build several small messages itself rather than hand a whole buffer to sendPacket. Both
     * go through this so the header encoding and the sequence counter have one home.
     *
     * The buffer needs room for HEADER_MAX_LENGTH + length.
     *
     * @return bytes written
     */
    size_t encodeAudioMessage(uint8_t id, const uint8_t *samples, size_t length, uint8_t *out);

private:
    /* Security exchange, driven entirely here - see the .cpp for the flow. */
    void handleAuthPacket(const uint8_t *data, size_t length);
    void handleAuthAcknowledge();
    bool sendAuthFrame(const uint8_t *payload, size_t length);
    bool sendAuthPacket(uint8_t command, const uint8_t *payload, size_t length);
    bool sendAuthHostSecret();
    bool sendAuthFinish();

    /* Version 2: restarts the exchange after a device declines version 1. */
    bool sendAuthHostHello2();
    /* Version 2: agrees the secret and sends our half of the ECDH exchange. */
    bool sendAuthHostPubkey();
    bool extractPublicKey(const uint8_t *data, size_t length);

    /*
     * TLS-style P_hash over HMAC-SHA256, which is what the handshake derives everything with:
     * the master secret from the pre-master secret, and each side's finish value from that.
     */
    std::vector<uint8_t> computePrf(const char *label,
                                    const std::vector<uint8_t> &key,
                                    const std::vector<uint8_t> &seed,
                                    size_t length);

    uint8_t authLastSent = 0;
    std::vector<uint8_t> authRandomHost;
    std::vector<uint8_t> authRandomClient;
    std::vector<uint8_t> authPublicKey;
    std::vector<uint8_t> authMasterSecret;
    // The device's raw P-256 public key, kept only from its arrival until the secret is agreed
    std::vector<uint8_t> authPublicKeyClient2;
    // Whether the exchange in progress is version 2, which changes which commands are expected
    bool authVersion2 = false;

    /*
     * Every handshake message, ours and the device's, from the data header onward - the handshake
     * header and the trailer are excluded. Both sides hash this and compare, so a single byte out
     * of place fails the exchange. Kept as raw bytes rather than a running digest because the
     * finish messages need a hash of a prefix of it, which is far simpler to take from a buffer
     * than to snapshot out of a streaming hash.
     */
    std::vector<uint8_t> authTranscript;

    bool acknowledgePacket(Frame frame);
    bool acknowledgeChunk(const Frame &frame, uint32_t received, uint32_t remaining);
    bool handleChunk(const Frame &frame, uint32_t length, uint32_t offset, const Bytes &data);
    // Takes the device id rather than reading it from shared state, so a transfer completing
    // while another device's is in flight still dispatches to its own device
    void dispatchChunked(uint8_t deviceId, uint8_t command, const uint8_t *data, size_t length);
    uint8_t getSequence(bool accessory = false);

#ifdef _DEBUG
    /*
     * Reports a packet addressed to an accessory, which handlePacket() otherwise discards without
     * trace. Debug builds only.
     *
     * This exists to answer one question: whether a pad ever announces a second GIP client. Audio
     * lives on one - in xone the headset is its own client, never device 0 - so if a headset is
     * reachable at all, its announce arrives here and is currently thrown away silently. A pad
     * tested against this driver produced no such packet at all, and that negative result is only
     * worth anything if it can be re-checked rather than taken on trust.
     *
     * Observational: nothing here dispatches. Routing an accessory announce into the normal path
     * would reach Controller::initInput() and assign over a joinable rumble thread, which
     * terminates the process.
     */
    void logAccessoryPacket(const Frame &frame, size_t size, const char *where);

    // Rate limiter for the above. An accessory that streams would arrive as fast as input does.
    uint32_t accessoryPackets = 0;
#endif

    uint8_t sequence = 0x01;
    uint8_t accessorySequence = 0x01;
    /*
     * Security is a Unique pool in MS-GIPUSB 2.2.9, so it counts separately from the Command
     * class. A capture of a Windows host shows exactly that: metadata, set-state and LED go out as
     * 1, 2, 3 and the first security message is 1 again.
     */
    uint8_t securitySequence = 0x01;

    // Audio is its own data class, and MS-GIPUSB 2.2.10.3 makes the sequence a counter per class.
    // Sharing the command counter would make both streams' numbering jump around.
    uint8_t audioSequence = 0x01;
    SendPacket sendPacket;

    /*
     * Reassembly state for one fragmented transfer. Fragmented messages are rare - metadata and
     * security only - so the buffer is allocated when a transfer starts and released when it
     * completes, rather than kept around.
     */
    struct ChunkTransfer
    {
        std::vector<uint8_t> buffer;
        uint32_t length = 0;
        uint8_t command = 0;
        bool active = false;
    };

    /*
     * One per device id, because a pad's sub-devices transfer independently of each other.
     *
     * This was a single shared slot, which was wrong in a way that could not announce itself.
     * probeSubDevices() asks devices 1 through 7 for metadata in one burst, and a sub-device that
     * announces mid-burst answers its own request on top of that - so two fragmented transfers can
     * be in flight at once. Sharing one buffer let their fragments interleave into it at their own
     * offsets, and the guard that would have caught it compares the *command*, which is
     * CMD_IDENTIFY for both. What came out was one plausible-looking metadata blob assembled from
     * two devices.
     *
     * Per device id is xone's model rather than an invention: chunk_buf_in and chunk_buf_out live
     * on struct gip_client, which is per device id.
     *
     * Sized for the full three-bit expansion index (2.2.10.2), and indexed with that mask, so a
     * malformed header cannot reach past the end. Eight empty vectors is what an idle pad costs.
     */
    static const size_t CHUNK_DEVICE_COUNT = 8;

    std::array<ChunkTransfer, CHUNK_DEVICE_COUNT> chunks;

    ChunkTransfer &chunkFor(uint8_t deviceId)
    {
        return chunks[deviceId & (CHUNK_DEVICE_COUNT - 1)];
    }
};

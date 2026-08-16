# Wire protocol

Bespoke. Both ends are ours, so there is no compatibility constraint — but the
format must be pinned here or successive sessions will invent inconsistent
variants of it.

Single UDP socket, single port, bidirectional. Video/audio unreliable, control
over a minimal reliable layer on the same socket. One socket keeps NAT and
firewall config trivial.

All integers little-endian. Payload ≤1200 bytes to stay under path MTU.

---

## Common header (12 bytes)

```
0       1       2       3
+-------+-------+-------+-------+
| type  | flags |   frame_id    |   type: u8, flags: u8, frame_id: u16
+-------+-------+-------+-------+
|         qpc_timestamp         |   u32, capture time, low bits of QPC
+-------------------------------+
|  pkt_idx      |   pkt_count   |   u16, u16
+-------------------------------+
```

`type`: 0=Video 1=Audio 2=Input 3=Control 4=Nack 5=Feedback 6=Rumble

`flags`: bit0 keyframe/IDR, bit1 last-packet-of-frame, bit2 unit-boundary
(NAL for HEVC, OBU for AV1), bit3 intra-refresh-active, bits4-7 reserved.

`frame_id` wraps at 16 bits. ~18 minutes at 60fps; handle wrap in the jitter
buffer with modular comparison, not `<`. It **increases monotonically** — the
instrumentation's frame table relies on that to evict stale slots, so a sender
that reuses or rewinds ids silently corrupts the timing report.

`qpc_timestamp` is the *sender's* clock and has no shared epoch with the
receiver. The delay-gradient controller does not need one, which is part of why
it was chosen, but end-to-end attribution does — see `clock_offset_ns` in
`Hello`/`SessionConfig` below.

---

## Video

Header + payload. Payload is a fragment of a slice (HEVC) or tile (AV1),
emitted by subframe readback as soon as the encoder produces it — do not wait
for end-of-frame.

- **HEVC**: Annex-B, start codes preserved. Set unit-boundary on the first
  fragment of each NAL.
- **AV1**: raw OBUs, **no start codes**. Respect `obu_has_size_field`. A temporal
  unit maps to one `frame_id`. Set unit-boundary on the first fragment of each OBU.

Sequence headers (VPS/SPS/PPS, or AV1 sequence header OBU) are sent over the
reliable control channel at session start, not inline — the client needs them to
configure MediaCodec before any frame arrives.

---

## Input (authenticated)

**Every input and control packet carries a MAC. Non-negotiable.** An
unauthenticated UDP port that calls `SendInput` is remote input injection for
anyone on the network.

```
common header
u32  input_seq          strictly increasing, replay window 256
u8   input_kind         0=Gamepad 1=KeyDown 2=KeyUp 3=MouseMove
                        4=MouseButton 5=MouseWheel
...  payload            per kind
u64  mac                keyed BLAKE3 over the whole preceding packet
```

**The MAC is authenticating but deterministic**, as every MAC is: the same key
over the same bytes gives the same tag. That is exactly why the tag alone is not
enough. The tag proves origin and integrity; `input_seq` and the replay window
provide freshness. Neither half is optional.

**Session keys, not the pairing key.** The `pairing_secret` below is long-lived;
the packet key is not. Deriving the packet key straight from the pairing secret
leaves a hole: `input_seq` restarts at zero each session, so a packet captured in
one session replays cleanly into the next — valid MAC, sequence above the window
floor, accepted. Both sides send a random 128-bit nonce during the handshake and
derive

```
session_key = BLAKE3::derive_key("sunburst session v1",
                                 pairing_secret ‖ client_nonce ‖ server_nonce)
```

Captured packets then fail verification in any later session. Held in memory
only.

**Verify the MAC before the replay window**, not after. Checking the sequence
first looks like a cheap DoS filter but inverts the ordering that matters, and
the MAC costs about 100ns. Compare tags in constant time. Reject silently and
without logging in the hot path.

BLAKE3 rather than SipHash for an integration reason rather than a speed one:
the same primitive does the KDF above and the per-packet MAC. Hardware-accelerated
AES-GMAC was considered and declined — at ~1,000 authenticated packets/sec the
MAC is about 0.01% of a core, so it would buy ~80µs/sec in exchange for runtime
CPU feature detection on three targets and a nonce-reuse failure mode that leaks
the authentication subkey. Video is unauthenticated by design, so nothing here
runs at a rate where hardware crypto pays.

Payloads:
- **Gamepad**: `pad_index u8`, buttons `u16`, LX/LY/RX/RY `i16`, LT/RT `u8`.
  Maps to ViGEm X360 report. The index is not optional: two Bluetooth pads need
  it and the Xbox Wireless Adapter's four make it unavoidable.
- **KeyDown/KeyUp**: Windows VK `u16` + modifier bitfield `u8`. Server derives the
  scancode via `MapVirtualKeyW(vk, MAPVK_VK_TO_VSC_EX)`. Do **not** send Android
  `getScanCode()` — those are Linux evdev codes and the mapping isn't clean.
- **MouseMove**: `mode: u8` (0=relative 1=absolute), then `dx/dy: i16` or
  `x/y: u16` normalised 0–65535.
- **MouseButton**: `button: u8` (0=L 1=R 2=M 3=X1 4=X2), `down: u8`.
- **MouseWheel**: `delta: i16` (`WHEEL_DELTA` 120), `horizontal: u8`.

---

## Control (reliable channel, authenticated)

Minimal reliable layer over the same socket: sequence, ack, retransmit on timeout.
~150 lines. Not a general-purpose stream — messages are small and infrequent.

### Reliable framing

```
common header (type=3)
u16  ctrl_seq     this frame's sequence
u16  ctrl_ack     highest contiguous sequence received from the peer
u8   ctrl_flags   bit0: carries a payload
...  message      one control message, envelope included
u64  mac          absent only for the pairing exchange
```

Acks are cumulative and piggyback on any outgoing message; a bare ack goes out
when there is nothing else to say. Window of 8 outstanding, retransmit at 200ms,
peer declared gone after 8 attempts.

**Delivery is in order, which is the opposite of video and deliberate.** Pairing
is a three-step exchange, and a `PairConfirm` overtaking its `PairRequest` would
reach a handler with no pending request to attach to. Anything arriving early is
held; anything further ahead than the window is dropped, or a peer could withhold
one sequence and grow the holding area without bound.

### Message envelope

```
u8   kind         ClientMessage or ServerMessage, by direction
u16  len
...  payload
```

The length is what lets a receiver **skip a kind it does not decode** instead of
desynchronising, which is what allows a newer client to talk to an older server
with no version negotiation.

### Which key verifies a packet

The common header carries no client id. The server tries each paired client's
key on the first authenticated packet from an address and remembers the answer;
a handful of clients at ~100ns each makes the scan cheap and it happens once.
`Hello.client_id` only orders that scan — a hint, not a credential.

**Replay state follows the client, not the address.** A MAC is deterministic, so
a captured packet resent from a different source port verifies perfectly; a
per-address replay window would be created fresh for that port and accept it.

Client → server:
- `PairRequest` — name, model, ABI, quirks, `client_nonce`. **Unauthenticated;
  see Pairing below.**
- `PairConfirm` — `request_id`, `tag`. Also unauthenticated.
- `ListApps` — ask for the catalogue
- `LaunchApp` — `app_id` from the last `AppList`
- `Hello` — client capabilities, ABI, display info, `client_nonce`, `clock_offset_ns`
- `DecoderQuirks` — the quirks struct (see CLAUDE.md); server adapts encoder config
- `RequestIdr` — last resort only; prefer NACK + reference invalidation
- `Resize` — client resolution/refresh change
- `PadConnected` — `pad_index`, type, capabilities. Server plugs a ViGEm target.
- `PadDisconnected` — `pad_index`. Server unplugs it.
- `Bye`

Server → client:
- `PairChallenge` — `request_id`, `server_nonce`. Unauthenticated.
- `AppList` — `(app_id, name)` pairs. **Names and ids only:** box art is a later
  phase, and a reliable control channel is the wrong carrier for image payloads.
- `SessionConfig` — codec, resolution, fps, bitrate, HDR metadata, `server_nonce`,
  `clock_offset_ns`
- `CodecPrivate` — VPS/SPS/PPS or av1C record. Client cannot configure MediaCodec
  without this. **av1C construction is a known silent-failure point:** wrong record
  means the decoder configures successfully and outputs nothing.
- `CursorShape` — bitmap + hotspot, for client-side cursor rendering
- `CursorPosition` — sent on absolute-mode changes only
- `SecureDesktop` — capture unavailable; client shows a placeholder rather than a
  frozen frame
- `Bye`

`clock_offset_ns` is estimated from the handshake round trip, accurate to about
RTT/2 — sub-millisecond on a wired LAN, which is ample for attributing 5–10ms
stages. Pad connect/disconnect go over the reliable channel deliberately: they
are infrequent and must not be lost, which is the opposite of rumble below.

`ListApps` works before a video session exists, which is what listing before
streaming requires.

---

## Pairing

Produces the `pairing_secret` every session key derives from. It is the root of
trust for the input path, so it is worth being precise about what this does and
does not achieve.

**The PIN never crosses the wire.** The client generates it and displays it on
the TV; the user types it into the web UI. Both ends then derive the same secret
independently:

```
pairing_secret = BLAKE3::derive_key("sunburst pairing v1",
                                    pin ‖ client_nonce ‖ server_nonce)
```

The obvious alternative — server mints the PIN, client sends it back — puts the
PIN in a packet, where a passive listener reads it directly and does not have to
guess at all. This direction is also the easy one to type: eight digits into a
browser rather than into a TV remote.

**Pairing packets are the one exception to "every control packet carries a MAC",**
because before pairing there is no key. What keeps that bounded:

- Only accepted while pairing is **armed from the web UI**. Never always-listening.
- The arming is **single use** and expires after **90 seconds**.
- PIN attempts are **capped at five**, after which the request is discarded.
- Anything arriving unarmed is dropped silently. An unsolicited pair request is
  the normal state of the world, not an incident.

Sequence:

1. Web UI arms. The server generates `server_nonce`.
2. Client sends `PairRequest`; server replies `PairChallenge` with the nonce.
3. Client derives the secret from its PIN and sends `PairConfirm` with
   `tag = BLAKE3::keyed_hash(secret, "sunburst pair confirm v1")[..8]`.
4. User types the PIN. The server derives a candidate secret and compares tags.
   A mismatch costs an attempt, not the arming — a typo should not mean walking
   back to the TV.

**What this is not.** It is not a key exchange. Anyone who captures a pairing
exchange *and* one later authenticated packet can grind the eight-digit PIN
offline and recover a secret that stays valid until revoked. That is a
deliberate choice for a LAN-only threat model, consistent with declining
encryption for video on the same network — but it is the weakest link in the
input path, and revoking a client must genuinely delete its secret. Upgrading to
X25519 would be a two-crate change confined to this section.

---

## Rumble (server → client, unreliable)

```
common header (type=6)
u8   pad_index
u16  motor_low          large/low-frequency motor
u16  motor_high         small/high-frequency motor
u8   rumble_seq         wraps; latest-wins
u64  mac
```

**Deliberately not on the reliable channel.** A superseded rumble level is
worthless, so latest-wins beats guaranteed delivery — retransmitting a level the
game has already moved past is strictly worse than dropping it. `rumble_seq`
exists so reordering cannot leave a motor stuck at a stale value; compare it
modularly and discard anything older than the last applied.

**Stop on silence.** An unreliable channel means the final zero-level packet can
be lost, and the failure mode is a controller that buzzes until the battery dies.
The client stops any motor it has heard nothing about for 200ms, and the server
repeats the current level every 100ms while it is non-zero.

Trigger rumble is not carried. XInput has no API for it, so there would be
nowhere on the server for it to go.

---

## NACK

```
common header (type=4)
u16  frame_id
u16  count
u16  missing_pkt_idx[count]
```

Server responds by calling `NvEncInvalidateRefFrames` for the affected frame and
continuing from the last client-confirmed reference — **not** by emitting an IDR.
This is the core latency advantage over stock Moonlight: recovery becomes nearly
invisible instead of a bitrate spike and visible hitch.

Suppressed when `DecoderQuirks.ref_invalidation == false` (Amlogic decoders are
known to mishandle it); fall back to `RequestIdr` on those devices.

HEVC and AV1 need **separate** invalidation state machines. AV1's reference model
— 8 slots with explicit signalling — is different enough that sharing the
implementation will produce subtle corruption.

---

## Feedback (client → server, every 100ms)

```
common header (type=5)
u32  recv_timestamp      client QPC-equivalent
u32  frames_received
u32  frames_dropped
u16  jitter_buffer_ms
u16  decode_p99_us
i32  owd_gradient        one-way delay gradient, µs/s
```

Drives rate control. **Delay gradient, not loss** — the gradient rises as queues
build, before any packet is dropped, so the controller reacts earlier. Applies via
`NvEncReconfigureEncoder`, which changes bitrate without tearing down the session.

---

## Not implemented, deliberately

- **FEC / Reed-Solomon.** On a wired LAN at sub-ms RTT, NACK + reference
  invalidation beats it on latency *and* bandwidth. Revisit only if a wireless
  client is ever added and RTT exceeds ~15–20ms.
- **Video encryption.** LAN-only, and the threat model doesn't justify the key
  management. Input and control are authenticated, which is where the actual risk is.
- **QUIC.** RFC 9221 datagrams are congestion-controlled, so QUIC's CC would fight
  the delay-gradient controller, and its pacer optimises for RTT rather than frame
  deadline. Also mandates TLS, which we've declined. See CLAUDE.md.

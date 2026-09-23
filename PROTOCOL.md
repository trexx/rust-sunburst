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
7=PadOutput 8=AudioIn. Every type except Video, Audio and AudioIn carries a MAC —
those three are media (LAN-only), and AudioIn is the client→server pad-headset mic.

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
- **H.264**: Annex-B NAL, exactly like HEVC (start codes preserved,
  unit-boundary per NAL), so it shares the video packetizer. **8-bit SDR only** —
  NVENC has no 10-bit H.264, so H.264 never carries HDR (`SessionConfig.hdr` is
  absent). An opt-in, low-latency codec; HDR stays on HEVC/AV1.

Sequence headers (VPS/SPS/PPS for HEVC or H.264, or the AV1 sequence header OBU)
are sent over the
reliable control channel at session start, not inline — the client needs them to
configure MediaCodec before any frame arrives.

---

## Audio

Header + one Opus packet. Server → client, **unauthenticated** (LAN-only, like
video). One Opus frame per datagram — at 48 kHz/5 ms a packet is a few hundred
bytes, well under `MAX_PAYLOAD`, so there is **no fragmentation and no
reassembler**: the whole audio wire path is `Header::encode` + payload.

Header fields for audio:

- `frame_id` — the **audio** sequence number, its own counter, unrelated to video
  `frame_id`s.
- `qpc_timestamp` — low 32 bits of the capture-time performance counter of the
  frame's first sample, in the **same clock domain as video**, so the client
  aligns audio against video with the `qpc_freq_hz` it already has.
- `pkt_idx` = 0, `pkt_count` = 1, `flags` = empty.

The stream is fixed at 48 kHz stereo, 5 ms frames, encoded
`OPUS_APPLICATION_RESTRICTED_LOWDELAY` with in-band FEC on. There is **no audio
NACK**: a lost packet is recovered from the next packet's FEC or concealed by the
decoder's PLC, both cheaper than a retransmit round trip that would exceed the
frame duration. The server injects a silence frame whenever the capture endpoint
is idle (WASAPI loopback delivers nothing then), so the cadence — and the A/V
sync anchor — never stops. Opus needs no per-stream codec-private, so there is no
audio `CodecPrivate`; the decoder is configured from `SessionConfig.audio` alone.

## AudioIn (client → server, unauthenticated)

Header + one Opus packet, the mirror of Audio in the other direction: an Xbox
pad's **headset microphone**, forwarded from the client so the server can play it
into a Windows microphone. Same framing rules as Audio — one Opus frame per
datagram, no fragmentation, no NACK — and **unauthenticated** for the same reason
(it is media, LAN-only; it makes the server *play a sound*, not *act*, so it does
not justify per-packet key management the way input and control do). The server
attributes it to the active session by source address and drops it from anywhere
else.

Header fields for audio-in:

- `type` = 8 (`AudioIn`).
- `frame_id` — the mic's own audio sequence number.
- `qpc_timestamp` — a client-side capture timestamp; the server renders the mic
  immediately and does not use it for sync today.
- `pkt_idx` = **pad index** — the stream id, since one session can carry more than
  one headset (≤2). `pkt_count` = 1, `flags` = empty.

The mic travels at its **native capture format** — an Xbox chat headset is
typically 24 kHz mono (`[MS-GIPUSB]` §3.2.5.1.2 format code `0x09`), distinct from
the 48 kHz-stereo speaker — and the client Opus-encodes it at that rate. The
server decodes with a 48 kHz-stereo Opus decoder, which resamples and up-mixes on
decode (Opus is 48 kHz internally; a stereo decoder up-mixes a mono stream), so
there is **no resampler** and the no-resample rule stays intact. The decoded audio
is rendered to a **consumed virtual microphone** (a signed "Steam Streaming
Microphone" endpoint selected by name — no driver of ours), the inbound twin of
the "Steam Streaming Speakers" capture-side reuse.

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
session_key = BLAKE3::derive_key("sunburst 2026 session key v1",
                                 pairing_secret ‖ client_nonce ‖ server_nonce)
```

Captured packets then fail verification in any later session. Held in memory
only. `Hello` carries `client_nonce`; `SessionConfig` carries `server_nonce`, so
`SessionConfig` itself — and any retransmit of it — is signed with the pairing
key, and both ends switch once it has crossed. How the switch is sequenced is
under *Which key verifies a packet* below.

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
- **Gamepad**: a controller-agnostic superset, presence-flagged. The core is
  always present; a 1-byte mask gates optional rich sections, so an Xbox pad stays
  ~16 bytes and a DualSense ~44.

  ```
  u8   pad_index      not optional: two Bluetooth pads need it, the Xbox adapter's four make it unavoidable
  u8   presence       bit0 IMU, bit1 touchpad, bit2 battery; bits3-7 reserved 0
  u32  buttons        low 16 are XInput's own bits; bits16+ are LEFT/RIGHT_PADDLE, *_PADDLE2,
                      TOUCHPAD_CLICK, SHARE, MISC1 — controls XInput has no name for
  i16  LX LY RX RY
  u8   LT RT
  -- if IMU:      i16 gyro pitch/yaw/roll (dps×16), i16 accel x/y/z (g×4096), u32 sensor_timestamp  (16 bytes)
  -- if touchpad: two fingers, each { u8 id|lifted-bit, u16 x, u16 y }             (10 bytes)
  -- if battery:  u8 level, u8 flags (bit0 charging, 1 full, 2 mic_muted, 3 headphones)  (2 bytes)
  ```

  A finger's byte 0 is `id & 0x7F`, with bit 7 set when the finger is **not**
  touching — the convention the HIDMaestro codec's touchpad field uses, so the
  server forwards it unchanged. The **d-pad lives in the four XInput direction
  bits**; there is no separate hat field — the server derives the codec's hat
  octant from them. The core maps onto a ViGEm X360 report; that is the fallback
  when the native driver is unavailable, not the primary target. The primary
  target is the HIDMaestro codec, which packs this into whatever profile the pad
  emulates (Xbox, DualSense, Switch Pro, …).

  A pad's *capability* to send each rich section is announced once at connect
  (`PadConnected.capabilities`, below); the per-frame presence mask is which
  sections a given packet actually carries.
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
- `Hello` — client capabilities, ABI, display info, `client_nonce`, `clock_offset_ns`,
  and `codecs`, a bitmask of what the client can decode (bit0 HEVC Main10,
  bit1 AV1 Main10, bit2 H.264 High 8-bit) from its `MediaCodecList` enumeration.
  The server never picks a codec the client did not offer, and never auto-selects
  H.264 over an HDR-capable codec (it is chosen only by preference or as a sole
  offer). Two optional request fields follow, both set from the TV settings
  screen and both advisory — the server clamps them and the client cannot force
  an unsupported or over-budget stream:
  - `prefer_codec` — one byte: a `StreamCodec` discriminant (0 HEVC, 1 AV1,
    2 H.264) the client would like, or `0xFF` for "no preference, server
    chooses". It only ranks ahead of the server's configured preference among
    the codecs `codecs` already advertises; a codec the device cannot decode is
    ignored, not honoured.
  - `max_bitrate_kbps` — u32, a client-side ceiling in kbps (`0` = none). The
    session bitrate is the minimum of the server/per-app setting, the codec
    ceiling, and this. It can only lower the rate, never raise it past the
    server's own cap.
- `DecoderQuirks` — the quirks struct (see CLAUDE.md); server adapts encoder config
- `RequestIdr` — last resort only; prefer NACK + reference invalidation
- `Resize` — client resolution/refresh change
- `PadConnected` — `pad_index`, `pad_type`, `capabilities`. The per-connection
  profile selector: `pad_type` says which controller the pad emulates — the server
  emulates the same family the client reports. Codes (`sunburst_input::pad::registry`):
  **0 = Xbox 360** (universal fallback), **1 = Xbox Series X|S**, **2 = DualShock 4**,
  **3 = DualSense**, **4 = Switch Pro**; more families extend the list. `capabilities` says which rich
  sections the pad can send (IMU, touchpad, battery, adaptive triggers). Sent
  reliably at connect, so the per-frame gamepad packet carries no profile byte —
  only the presence mask for what *this* packet holds.
- `PadDisconnected` — `pad_index`. Server unplugs it.
- `Bye`

Server → client:
- `PairChallenge` — `request_id`, `server_nonce`. Unauthenticated.
- `AppList` — `(app_id, name)` pairs. **Names and ids only:** box art is a later
  phase, and a reliable control channel is the wrong carrier for image payloads.
- `SessionConfig` — everything the client needs before the first frame. Sent
  once per session, signed with the pairing key (it carries the nonce the
  session key derives from):

  ```
  u32  session_id
  u8   codec           0 = HEVC Main10, 1 = AV1 Main10, 2 = H.264 High 8-bit SDR
  u16  width, u16 height
  u32  fps_mhz         millihertz, same unit as Hello.refresh_mhz
  u32  bitrate_kbps    initial target; the rate controller moves it afterwards
  u8   flags           bit0 hdr, bit1 intra_refresh_on, bit2 ref_invalidation_on, bit3 audio_on
  u8   slices          subframe units per frame: HEVC/H.264 slices, or AV1 tiles
  [16] server_nonce
  u64  qpc_freq_hz     ticks per second behind the video header's qpc_timestamp
  i64  server_ns       server clock when this was queued
  u32  hello_delay_ns  server time between Hello receipt and this message
  -- if hdr: ST 2086 mastering block, chromaticity in 0.00002 steps, luminance in 0.0001 cd/m²
  u16  rx ry gx gy bx by wx wy   u32 max_lum  u32 min_lum   u16 max_cll  u16 max_fall
  -- if audio_on (bit3): Opus stream parameters
  u32  sample_rate     48000
  u8   channels        2
  u16  frame_samples   samples per channel per packet (240 = 5 ms at 48 kHz)
  ```

  `flags` bits 1–2 say what the server will *actually do*, which is the quirks
  the client reported intersected with what the encoder supports. The clock
  fields are what make `clock_offset_ns` derivable: the client subtracts
  `hello_delay_ns` from its measured Hello→SessionConfig round trip, so server
  processing time (building the encoder) does not inflate the estimate.
- `CodecPrivate` — `u8 codec, u16 len, bytes`: VPS/SPS/PPS (Annex-B) or the
  av1C record. Sent after `SessionConfig` and **again after any encoder
  rebuild**. Client cannot configure MediaCodec without this. **av1C
  construction is a known silent-failure point:** wrong record means the decoder
  configures successfully and outputs nothing.
- `CursorShape` — for client-side cursor rendering. A bitmap is larger than one
  reliable message, so it is **chunked**; every chunk repeats the head, and the
  in-order channel means the receiver appends and never reorders:

  ```
  u32  shape_id        increments per new shape; width = 0 means hidden
  u16  width, u16 height, u16 hotspot_x, u16 hotspot_y
  u8   format          0 = BGRA32 premultiplied
  u32  total_len       pixel bytes for the whole shape
  u32  offset          this chunk's byte offset
  u16  chunk_len       ≤ 1024
  ...  chunk
  ```
- `CursorPosition` — `u16 x, u16 y` (0–65535, normalised to the captured
  monitor), `u8 visible`. A correction, not the primary source: the client moves
  its own cursor from its own input, and this arrives when the server-observed
  position disagrees (a warp, or absolute mode). Throttled to ≤10/s.
- `SecureDesktop` — `u8 active`. Capture unavailable (UAC, lock screen, DRM);
  the client shows a placeholder rather than a frozen frame, and `active = 0`
  ends it.
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
u16  trigger_left       Xbox left impulse-trigger motor (0 if none)
u16  trigger_right      Xbox right impulse-trigger motor
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

This packet carries **four** motor levels — the two rumble motors plus the Xbox
**impulse-trigger** motors (`trigger_left`/`trigger_right`, zero for a pad without
them). A native pad's richer effects (lightbar, adaptive triggers, player LEDs)
ride **`PadOutput`** (type 7) below; the two are never sent for the same
controller. Rumble stays its own type because a motor level is continuous and
latest-wins, which suits a simple pad, the X360 fallback path, and the Xbox pad's
four motors driven straight through the vendored GIP bridge.

---

## PadOutput (server → client, unreliable)

```
common header (type=7)
u8   pad_index
u8   pad_seq              wraps; latest-wins, like rumble
u16  motor_low
u16  motor_high
u8   led[3]               lightbar RGB
u8   player_led           family-specific player-indicator pattern
u8   flags                bit0 mic-mute LED; bits1-7 reserved
u8   left_len             adaptive-trigger effect length (≤11)
...  left_effect[left_len]
u8   right_len
...  right_effect[right_len]
u64  mac
```

A native pad's output report sets more than a motor level — a DualSense frame
carries both motors, the two adaptive-trigger effects, the lightbar and the
player LEDs at once. The server decodes the game's output report (via the
HIDMaestro codec) and ships that union here, so the client applies whatever its
hardware supports and drops the rest. **The trigger effects are opaque,
length-prefixed, family-specific blobs** (a DualSense effect is 11 bytes); the
server does not model their semantics. **Unreliable and latest-wins** for the same
reason as rumble — a superseded effect is worthless — with `pad_seq` guarding
against reordering. Simple pads use `Rumble`; rich pads use this.

---

## NACK (client → server, unreliable, authenticated)

```
common header (type=4, frame_id = the frame in question)
u16  count               0 = abandoned (see below)
u16  missing_pkt_idx[count]
u64  mac
```

Two meanings, by `count`:

- **`count > 0` — retransmit.** The client has the terminator (so it knows the
  total) or a gap below the highest index seen, and lists what is missing. The
  server resends those packets from a cache of the last few frames. At sub-ms
  LAN round trips the resend lands well inside the jitter-buffer deadline, so the
  frame still displays on time and the encoder is never involved. This is the
  common case and it costs nothing visible.
- **`count == 0` — abandoned.** The client gave up on the frame: the jitter
  buffer stepped over it, or it was evicted before completing. The server calls
  `NvEncInvalidateRefFrames` for that frame **and every frame encoded since**,
  and continues from the last good reference — **not** by emitting an IDR. It
  forces an IDR only when nothing decodable is left in the DPB. This is the core
  latency advantage over stock Moonlight: recovery becomes nearly invisible
  instead of a bitrate spike and visible hitch.

Authenticated because an abandon makes the encoder do work; an unauthenticated
one would let anyone on the network force IDRs at will. There is no sequence
number: a NACK for a frame older than the cache is simply unanswerable, and a
replayed abandon at worst repeats an invalidation the state machine already
ignores.

Invalidation is suppressed when `DecoderQuirks.ref_invalidation == false`
(Amlogic decoders are known to mishandle it); an abandon then forces an IDR
instead. Retransmit is never suppressed.

HEVC and AV1 need **separate** invalidation state machines. AV1's reference model
— 8 slots with explicit signalling — is different enough that sharing the
implementation will produce subtle corruption. H.264, by contrast, uses the same
sliding-DPB model as HEVC and **shares** its state machine.

---

## Feedback (client → server, every 100ms, authenticated)

```
common header (type=5)
u32  recv_timestamp      client clock, ns, low 32 bits
u32  frames_received
u32  frames_dropped
u16  jitter_buffer_ms
u16  decode_p99_us
i32  owd_gradient        one-way delay gradient, µs/s
u64  mac
```

`owd_gradient` is the least-squares slope of `(recv_ns − send_ns)` over the last
window, where `send_ns` is the video header's `qpc_timestamp` converted with
`SessionConfig.qpc_freq_hz`. A constant clock offset between the machines
differentiates to zero, which is why no shared epoch is needed. Authenticated
because a forged feedback could drive the bitrate to the floor.

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

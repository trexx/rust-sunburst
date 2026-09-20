// SPDX-License-Identifier: GPL-2.0-or-later

//! The UDP endpoint: one socket, demultiplexed by packet type.
//!
//! `std::net::UdpSocket` on a dedicated thread with a read timeout, so `tick`
//! runs without a runtime. CLAUDE.md keeps async off this path.
//!
//! # Which key verifies a packet
//!
//! The common header has no client id, and adding one would be a protocol
//! change. Instead the endpoint tries each paired client's key on the first
//! authenticated packet from an address, then remembers the answer. There are a
//! handful of clients and the MAC is about 100ns, so the scan is cheap and it
//! happens once per address.
//!
//! `Hello` carries a `client_id`, which is used only to order that scan. It is a
//! hint, not a credential: the MAC is what proves identity.
//!
//! # State follows the client, not the address
//!
//! The replay window and the reliable channel are keyed by **client id**, and
//! the source address is only a return path that a session updates as it moves.
//!
//! Keying them by address instead is a hole, and not a subtle one: a MAC is
//! deterministic, so anyone who captures an authenticated input packet can
//! resend it from a different source port. A per-address window would be created
//! fresh for that port, the captured packet would verify, and the replay would
//! be accepted. Keying by client means the window that already saw that sequence
//! is the one consulted, wherever the packet came from.
//!
//! # Known gap: the key is the pairing secret, not a session key
//!
//! PROTOCOL.md specifies a per-session key derived from the pairing secret and a
//! nonce from each side. This endpoint verifies against the pairing secret
//! directly, because the message that would carry `server_nonce` is
//! `SessionConfig`, whose fields Phase 3 has not decided.
//!
//! What that leaves open, stated rather than buried: the replay window lives in
//! memory, so **after a server restart a captured input packet can be replayed
//! once**. Within a run it cannot — the window follows the client, per above.
//! Session keys close it, and they land with `SessionConfig`.
//!
//! # Pairing is the one unauthenticated path
//!
//! Before pairing there is no key, so `PairRequest` and `PairConfirm` arrive
//! without a MAC. They are processed only while the handler reports pairing
//! armed, only for those two kinds, and dropped silently otherwise. Those peers
//! are tracked by address, because they have no identity yet.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sunburst_core::proto::padoutput::PAD_OUTPUT_MAX_BODY;
use sunburst_core::proto::pairing::NONCE_LEN;
use sunburst_core::proto::rumble::RUMBLE_BODY_LEN;
use sunburst_core::proto::{
    ClientControl, ClientMessage, Feedback, Flags, HEADER_LEN, Header, InputPacket, MAC_LEN,
    MAX_PAYLOAD, Nack, PacketType, PadOutput, ReplayWindow, Rumble, Seq16, ServerControl,
    SessionConfig, SessionKey,
};

use crate::handler::{ControlHandler, Outbound};
use crate::reliable::{FRAME_HEADER_LEN, Reliable, ReliableError};

/// Room for a control message once the common header, the reliable frame header
/// and the MAC are accounted for.
pub const MAX_CONTROL_PAYLOAD: usize = MAX_PAYLOAD - FRAME_HEADER_LEN - MAC_LEN;

/// How long a peer may go silent before its state is dropped.
const IDLE_SECS: u64 = 120;

/// Read timeout, which is also how often `tick` runs.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Which channel a packet arrived on, and therefore which key must verify it.
///
/// The reliable **control** channel stays on the long-lived pairing key: the
/// message that establishes the session key (`SessionConfig`) travels it, so it
/// cannot itself require that key, and the control messages are not the input
/// path the session key exists to protect. **Data** — input, NACK, feedback —
/// switches to the per-session key the moment one is installed, which is what
/// makes a packet captured in one session fail in the next.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Channel {
    Control,
    Data,
}

/// An authenticated client. Survives a change of source address.
struct Session {
    /// The long-lived key derived straight from the pairing secret. Verifies the
    /// control channel and identifies the client on its first packet.
    pairing_key: SessionKey,
    /// The per-session key derived once `Hello` supplied the client nonce and
    /// `SessionConfig` the server nonce. Once set, the data channel verifies
    /// against this and the pairing key is no longer accepted for input.
    session_key: Option<SessionKey>,
    /// The client's handshake nonce, stashed from `Hello` so the session key can
    /// be derived when `SessionConfig` is sent.
    client_nonce: Option<[u8; NONCE_LEN]>,
    addr: SocketAddr,
    reliable: Reliable,
    replay: ReplayWindow,
    last_seen_ms: u64,
    /// Control messages that did not fit the reliable window when produced;
    /// drained in order as the window frees. A cursor bitmap is several chunks,
    /// so a burst can exceed the window, and dropping a chunk would corrupt the
    /// shape — the queue is what makes reliable delivery hold under a burst.
    pending_out: VecDeque<ServerControl>,
}

impl Session {
    /// The key the data channel (input, NACK, feedback, rumble, pad output)
    /// signs and verifies with: the session key once installed, else the
    /// pairing key for the window before `Hello` has completed.
    fn data_key(&self) -> &SessionKey {
        self.session_key.as_ref().unwrap_or(&self.pairing_key)
    }
}

/// A peer that has not authenticated. Pairing only.
struct Pending {
    reliable: Reliable,
    last_seen_ms: u64,
}

pub struct Endpoint<H: ControlHandler> {
    socket: UdpSocket,
    handler: H,
    sessions: HashMap<u32, Session>,
    /// Skips the key scan for an address already attributed to a client.
    by_addr: HashMap<SocketAddr, u32>,
    pending: HashMap<SocketAddr, Pending>,
    origin: Instant,
}

impl<H: ControlHandler> Endpoint<H> {
    pub fn bind(addr: SocketAddr, handler: H) -> io::Result<Endpoint<H>> {
        Endpoint::from_socket(UdpSocket::bind(addr)?, handler)
    }

    /// Build on an already-bound socket, so the video send path can hold a clone
    /// of the very same socket (one shared port). The caller clones before
    /// handing the socket over.
    pub fn from_socket(socket: UdpSocket, handler: H) -> io::Result<Endpoint<H>> {
        socket.set_read_timeout(Some(POLL_INTERVAL))?;
        Ok(Endpoint {
            socket,
            handler,
            sessions: HashMap::new(),
            by_addr: HashMap::new(),
            pending: HashMap::new(),
            origin: Instant::now(),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// A second handle on the endpoint's UDP socket, for the video send path.
    ///
    /// PROTOCOL.md puts video and control on one port, and the endpoint owns it
    /// for receive; the frame path sends from a clone so both leave the same
    /// source address without the two paths sharing a lock. `try_clone` dups the
    /// OS socket — the receive loop here and the send loop there refer to one
    /// underlying socket.
    pub fn try_clone_socket(&self) -> io::Result<UdpSocket> {
        self.socket.try_clone()
    }

    pub fn handler(&self) -> &H {
        &self.handler
    }

    /// One receive-or-timeout, then a tick. Returns whether a datagram arrived.
    pub fn poll_once(&mut self) -> io::Result<bool> {
        let mut buf = [0u8; HEADER_LEN + MAX_PAYLOAD];
        let received = match self.socket.recv_from(&mut buf) {
            Ok((len, from)) => {
                self.on_datagram(&buf[..len], from);
                true
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                false
            }
            // A refused datagram on a connectionless socket is normal on Windows
            // (an ICMP port-unreachable surfaces as ConnectionReset) and says
            // nothing about this socket's health.
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => false,
            Err(e) => return Err(e),
        };

        self.tick();
        Ok(received)
    }

    /// Loop until `stop` is set.
    pub fn run(&mut self, stop: &std::sync::atomic::AtomicBool) -> io::Result<()> {
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            self.poll_once()?;
        }
        Ok(())
    }

    fn now_ms(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }

    fn on_datagram(&mut self, datagram: &[u8], from: SocketAddr) {
        let Some(header) = Header::decode(datagram) else {
            return;
        };
        let body = &datagram[HEADER_LEN..];

        match header.packet_type {
            PacketType::Control => self.on_control(datagram, body, from),
            PacketType::Input => self.on_input(datagram, body, from),
            PacketType::Nack => self.on_nack(datagram, header.frame_id, body, from),
            PacketType::Feedback => self.on_feedback(datagram, body, from),
            PacketType::AudioIn => self.on_audio_in(datagram, from),
            // Video and audio flow the other way; rumble and pad output are
            // server-originated. Nothing else is expected inbound.
            _ => {}
        }
    }

    /// A pad's headset-mic Opus frame. Audio is unauthenticated (LAN, like the
    /// outbound audio and video), so there is no MAC to verify; the sender is
    /// attributed by source address to a client the endpoint already knows, and
    /// dropped if the address is unrecognised — so a stray host cannot inject
    /// microphone audio into a session it never joined.
    fn on_audio_in(&mut self, datagram: &[u8], from: SocketAddr) {
        let Some((pad_index, header, payload)) = crate::audio::parse_audio_in_packet(datagram)
        else {
            return;
        };
        let Some(&client) = self.by_addr.get(&from) else {
            return;
        };
        self.handler
            .on_audio_in(client, pad_index, header.frame_id, payload);
    }

    /// Identify the sender on `channel`, scanning the paired keys if the address
    /// is new. On success the session's return address is updated, so a client
    /// that reconnects from a new port keeps its replay window and its channel.
    ///
    /// The channel decides which key may verify. Control always uses the pairing
    /// key. Data uses the session key once one is installed and refuses the
    /// pairing key thereafter — so a captured session-key packet fails in any
    /// later session (the key differs) and a pairing-key packet cannot stand in
    /// for input once the switch has happened.
    fn authenticate(&mut self, datagram: &[u8], from: SocketAddr, channel: Channel) -> Option<u32> {
        let now_ms = self.now_ms();

        // Fast path: an address already attributed to a client.
        if let Some(&client) = self.by_addr.get(&from)
            && let Some(session) = self.sessions.get_mut(&client)
        {
            match (channel, &session.session_key) {
                (Channel::Data, Some(sk)) => {
                    if sk.verify_packet(datagram).is_some() {
                        session.last_seen_ms = now_ms;
                        return Some(client);
                    }
                    // The session key is installed, so a data packet that does
                    // not carry it is refused rather than falling back.
                    return None;
                }
                _ => {
                    if session.pairing_key.verify_packet(datagram).is_some() {
                        session.last_seen_ms = now_ms;
                        return Some(client);
                    }
                }
            }
        }

        // Scan the pairing keys for a new or reconnecting address.
        for (client, key) in self.handler.client_keys() {
            if key.verify_packet(datagram).is_none() {
                continue;
            }
            // A pairing-key data packet for a client that has already switched is
            // exactly the stand-in the switch forbids.
            if channel == Channel::Data
                && self
                    .sessions
                    .get(&client)
                    .is_some_and(|sess| sess.session_key.is_some())
            {
                return None;
            }

            match self.sessions.get_mut(&client) {
                Some(session) => {
                    // The same client from a different port. The replay window
                    // comes with it — that is the point of keying on the client.
                    // The old session key is dropped: a reconnect re-`Hello`s and
                    // gets a fresh one, and a stale key must not linger.
                    self.by_addr.remove(&session.addr);
                    session.addr = from;
                    session.last_seen_ms = now_ms;
                    session.pairing_key = key;
                    session.session_key = None;
                }
                None => {
                    self.sessions.insert(
                        client,
                        Session {
                            pairing_key: key,
                            session_key: None,
                            client_nonce: None,
                            addr: from,
                            reliable: Reliable::new(MAX_CONTROL_PAYLOAD),
                            replay: ReplayWindow::new(),
                            last_seen_ms: now_ms,
                            pending_out: VecDeque::new(),
                        },
                    );
                }
            }
            self.by_addr.insert(from, client);
            // An address that authenticates is no longer a pairing candidate.
            self.pending.remove(&from);
            return Some(client);
        }
        None
    }

    fn on_control(&mut self, datagram: &[u8], body: &[u8], from: SocketAddr) {
        let now_ms = self.now_ms();
        let now = unix_now();

        // Authenticated first. The unauthenticated pairing path is only reached
        // when no key verifies, and then only while armed.
        if let Some(client) = self.authenticate(datagram, from, Channel::Control) {
            let frame = &body[..body.len().saturating_sub(MAC_LEN)];
            let messages = {
                let session = self.sessions.get_mut(&client).expect("just authenticated");
                session.reliable.on_frame(frame)
            };
            for message in messages {
                self.on_authenticated(&message, client, from, now);
            }
            return;
        }

        if !self.handler.pairing_armed(now) {
            return;
        }

        let entry = self.pending.entry(from).or_insert_with(|| Pending {
            reliable: Reliable::new(MAX_CONTROL_PAYLOAD),
            last_seen_ms: now_ms,
        });
        entry.last_seen_ms = now_ms;
        let messages = entry.reliable.on_frame(body);

        for message in messages {
            self.on_unauthenticated(&message, from, now);
        }
    }

    /// Messages from a peer with no key. Pairing only.
    fn on_unauthenticated(&mut self, message: &[u8], from: SocketAddr, now: u64) {
        let Ok((decoded, _)) = ClientControl::decode(message) else {
            return;
        };

        // The allow-list. Anything else arriving without a MAC is an attempt to
        // skip authentication, not a peer that has not got round to it.
        if !decoded.kind().is_some_and(ClientMessage::is_pre_pairing) {
            return;
        }

        match decoded {
            ClientControl::PairRequest(request) => {
                if let Some((request_id, server_nonce)) = self.handler.on_pair_request(request, now)
                {
                    self.send_pending(
                        from,
                        &ServerControl::PairChallenge {
                            request_id,
                            server_nonce,
                        },
                    );
                }
            }
            ClientControl::PairConfirm { request_id, tag } => {
                self.handler.on_pair_confirm(request_id, tag, now);
            }
            _ => {}
        }
    }

    fn on_authenticated(&mut self, message: &[u8], client: u32, from: SocketAddr, _now: u64) {
        let Ok((decoded, _)) = ClientControl::decode(message) else {
            return;
        };

        match decoded {
            ClientControl::Hello(hello) => {
                // Stash the client nonce so the session key can be derived when
                // the config is sent, then offer the session the handler returns.
                if let Some(session) = self.sessions.get_mut(&client) {
                    session.client_nonce = Some(hello.client_nonce);
                }
                if let Some(config) = self.handler.on_hello(client, from, hello) {
                    self.offer_session(client, config);
                }
            }
            ClientControl::Quirks(quirks) => self.handler.on_quirks(client, quirks),
            ClientControl::RequestIdr => self.handler.on_request_idr(client),
            ClientControl::Resize {
                width,
                height,
                refresh_mhz,
            } => self.handler.on_resize(client, width, height, refresh_mhz),
            ClientControl::ListApps => {
                let apps = self.handler.on_app_list();
                self.send_to_client(client, &ServerControl::AppList(apps));
            }
            ClientControl::LaunchApp { app_id } => {
                let _ = self.handler.on_launch(app_id);
            }
            ClientControl::PadConnected {
                pad_index,
                pad_type,
                capabilities,
            } => self
                .handler
                .on_pad_connected(client, pad_index, pad_type, capabilities),
            ClientControl::PadDisconnected { pad_index } => {
                self.handler.on_pad_disconnected(client, pad_index)
            }
            ClientControl::Bye => {
                self.handler.on_bye(client);
                self.forget(client);
            }
            // Pairing messages from an already-authenticated peer are ignored:
            // it has a key, so it is not pairing.
            //
            // Everything else is reserved or not yet acted on. Decoding
            // succeeded, so the stream is intact.
            _ => {}
        }
    }

    fn on_input(&mut self, datagram: &[u8], body: &[u8], from: SocketAddr) {
        let Some(client) = self.authenticate(datagram, from, Channel::Data) else {
            return;
        };

        let payload = &body[..body.len().saturating_sub(MAC_LEN)];
        let Some(packet) = InputPacket::decode(payload) else {
            return;
        };

        let fresh = {
            let session = self.sessions.get_mut(&client).expect("just authenticated");
            // The MAC proved origin; this proves freshness. Both are needed,
            // since a MAC is deterministic and a captured packet re-sent
            // verbatim verifies perfectly.
            session.replay.accept(packet.input_seq)
        };
        if fresh {
            self.handler.on_input(client, packet.event);
        }
    }

    /// A NACK: authenticate on the data channel, then hand the body (MAC
    /// stripped) to the handler, which retransmits or invalidates.
    fn on_nack(&mut self, datagram: &[u8], frame_id: Seq16, body: &[u8], from: SocketAddr) {
        let Some(client) = self.authenticate(datagram, from, Channel::Data) else {
            return;
        };
        let payload = &body[..body.len().saturating_sub(MAC_LEN)];
        // Reject a malformed body rather than passing garbage on.
        if Nack::decode(payload).is_none() {
            return;
        }
        self.handler.on_nack(client, frame_id, payload);
    }

    /// A feedback report: authenticate on the data channel, decode, hand up.
    fn on_feedback(&mut self, datagram: &[u8], body: &[u8], from: SocketAddr) {
        let Some(client) = self.authenticate(datagram, from, Channel::Data) else {
            return;
        };
        let payload = &body[..body.len().saturating_sub(MAC_LEN)];
        if let Some(feedback) = Feedback::decode(payload) {
            self.handler.on_feedback(client, feedback);
        }
    }

    /// Sign, frame reliably, and send a `SessionConfig`, then install the derived
    /// session key. The config is signed with the pairing key — the client has
    /// not derived the session key yet, this is the message it derives it from —
    /// and once it is out, input switches to the session key on both ends.
    fn offer_session(&mut self, client: u32, config: SessionConfig) {
        let _ = self.try_offer_session(client, config);
    }

    fn try_offer_session(&mut self, client: u32, config: SessionConfig) -> Option<()> {
        let secret = self.handler.client_secret(client)?;
        let encoded = ServerControl::SessionConfig(config.clone()).encode().ok()?;
        let now_ms = self.now_ms();

        let (addr, frame, pairing_key, client_nonce) = {
            let session = self.sessions.get_mut(&client)?;
            let cn = session.client_nonce?;
            // The window is empty at session start, so this does not block; if it
            // somehow did, dropping is correct — the client re-`Hello`s.
            let frame = session.reliable.send(&encoded, now_ms).ok()?;
            (session.addr, frame, session.pairing_key.clone(), cn)
        };

        self.transmit(addr, &frame, Some(&pairing_key), PacketType::Control);

        let session = self.sessions.get_mut(&client)?;
        session.session_key = Some(SessionKey::derive(
            &secret,
            &client_nonce,
            &config.server_nonce,
        ));
        Some(())
    }

    fn forget(&mut self, client: u32) {
        self.handler.session_stop(client);
        if let Some(session) = self.sessions.remove(&client) {
            self.by_addr.remove(&session.addr);
        }
    }

    fn send_to_client(&mut self, client: u32, message: &ServerControl) {
        if let Some(session) = self.sessions.get_mut(&client) {
            session.pending_out.push_back(message.clone());
        }
        self.flush_control(client);
    }

    /// Send as many queued control messages as the reliable window allows, in
    /// order. Stops at the first `WouldBlock`; `tick` retries as the window
    /// frees. Control is signed with the pairing key throughout (see `Channel`).
    fn flush_control(&mut self, client: u32) {
        let now_ms = self.now_ms();
        let Some(session) = self.sessions.get_mut(&client) else {
            return;
        };
        let addr = session.addr;
        let key = session.pairing_key.clone();
        let mut frames: Vec<Vec<u8>> = Vec::new();
        while let Some(message) = session.pending_out.front() {
            let Ok(encoded) = message.encode() else {
                // Unencodable is our bug, not the peer's; drop it and move on.
                session.pending_out.pop_front();
                continue;
            };
            match session.reliable.send(&encoded, now_ms) {
                Ok(frame) => {
                    frames.push(frame);
                    session.pending_out.pop_front();
                }
                Err(ReliableError::WouldBlock) => break,
                Err(_) => {
                    session.pending_out.pop_front();
                }
            }
        }
        for frame in frames {
            self.transmit(addr, &frame, Some(&key), PacketType::Control);
        }
    }

    fn send_pending(&mut self, to: SocketAddr, message: &ServerControl) {
        let Ok(encoded) = message.encode() else {
            return;
        };
        let now_ms = self.now_ms();

        let Some(peer) = self.pending.get_mut(&to) else {
            return;
        };
        let Ok(frame) = peer.reliable.send(&encoded, now_ms) else {
            return;
        };
        self.transmit(to, &frame, None, PacketType::Control);
    }

    fn transmit(&self, to: SocketAddr, body: &[u8], key: Option<&SessionKey>, kind: PacketType) {
        let header = Header {
            packet_type: kind,
            flags: Flags::EMPTY,
            frame_id: Seq16(0),
            qpc_timestamp: 0,
            pkt_idx: 0,
            pkt_count: 1,
        };

        let mut datagram = vec![0u8; HEADER_LEN];
        header.encode((&mut datagram[..]).try_into().expect("HEADER_LEN bytes"));
        datagram.extend_from_slice(body);
        if let Some(key) = key {
            key.sign_packet(&mut datagram);
        }
        let _ = self.socket.send_to(&datagram, to);
    }

    /// Send one rumble packet, unreliably.
    ///
    /// Not through the reliable channel: a superseded level is worthless, so
    /// latest-wins beats guaranteed delivery. `type=6` is authenticated, so it
    /// is signed with the session key like any other. A client that has never
    /// authenticated has no session and nothing is sent.
    fn send_rumble(&self, client: u32, rumble: &Rumble) {
        let Some(session) = self.sessions.get(&client) else {
            return;
        };
        let mut body = [0u8; RUMBLE_BODY_LEN];
        if rumble.encode(&mut body).is_none() {
            return;
        }
        self.transmit(
            session.addr,
            &body,
            Some(session.data_key()),
            PacketType::Rumble,
        );
    }

    /// Send one rich pad-output packet, unreliably (`type=7`, authenticated).
    /// Same latest-wins reasoning as [`Self::send_rumble`], for rich pads.
    fn send_pad_output(&self, client: u32, output: &PadOutput) {
        let Some(session) = self.sessions.get(&client) else {
            return;
        };
        let mut body = [0u8; PAD_OUTPUT_MAX_BODY];
        let Some(n) = output.encode(&mut body) else {
            return;
        };
        self.transmit(
            session.addr,
            &body[..n],
            Some(session.data_key()),
            PacketType::PadOutput,
        );
    }

    /// Send whatever a producer queued since the last tick.
    ///
    /// The one place server-originated messages reach the socket; producers run
    /// on other threads and never touch it themselves.
    fn dispatch_outbound(&mut self) {
        for message in self.handler.drain_outbound() {
            match message {
                Outbound::Control { client, message } => self.send_to_client(client, &message),
                Outbound::Rumble { client, rumble } => self.send_rumble(client, &rumble),
                Outbound::PadOutput { client, output } => self.send_pad_output(client, &output),
            }
        }
    }

    /// Retransmits, owed acks, and dropping peers that have gone quiet.
    fn tick(&mut self) {
        self.dispatch_outbound();
        let now_ms = self.now_ms();
        let idle_ms = IDLE_SECS * 1000;
        let mut send: Vec<(SocketAddr, Vec<u8>, Option<SessionKey>)> = Vec::new();
        let mut drop_clients = Vec::new();
        let mut drop_pending = Vec::new();

        for (client, session) in &mut self.sessions {
            if now_ms.saturating_sub(session.last_seen_ms) > idle_ms {
                drop_clients.push(*client);
                continue;
            }
            match session.reliable.tick(now_ms) {
                Ok(frames) => {
                    for frame in frames {
                        send.push((session.addr, frame, Some(session.pairing_key.clone())));
                    }
                }
                Err(ReliableError::PeerGone) => drop_clients.push(*client),
                Err(_) => {}
            }
        }

        for (addr, peer) in &mut self.pending {
            if now_ms.saturating_sub(peer.last_seen_ms) > idle_ms {
                drop_pending.push(*addr);
                continue;
            }
            match peer.reliable.tick(now_ms) {
                Ok(frames) => {
                    for frame in frames {
                        send.push((*addr, frame, None));
                    }
                }
                Err(ReliableError::PeerGone) => drop_pending.push(*addr),
                Err(_) => {}
            }
        }

        for (addr, frame, key) in send {
            self.transmit(addr, &frame, key.as_ref(), PacketType::Control);
        }
        // Push out anything that was waiting on window space.
        let clients: Vec<u32> = self.sessions.keys().copied().collect();
        for client in clients {
            self.flush_control(client);
        }
        for client in drop_clients {
            self.forget(client);
        }
        for addr in drop_pending {
            self.pending.remove(&addr);
        }
    }
}

/// A client's side of the control channel, for the fake client and for tests.
///
/// Shares the framing and signing so the two ends cannot drift apart.
pub struct ClientEndpoint {
    pub socket: UdpSocket,
    pub server: SocketAddr,
    pub reliable: Reliable,
    /// Verifies incoming control and signs outgoing control. For a test with a
    /// fixed key and no handshake, this is that key; for a real client it is the
    /// pairing key.
    pub key: Option<SessionKey>,
    /// The raw pairing secret, kept so the session key can be derived when
    /// `SessionConfig` arrives. Set by [`connect_paired`](Self::connect_paired).
    secret: Option<[u8; 32]>,
    /// The nonce sent in the last `Hello`, half of the session-key input.
    client_nonce: Option<[u8; NONCE_LEN]>,
    /// Signs outgoing input/NACK/feedback once `SessionConfig` has arrived. Until
    /// then those use [`key`](Self::key), matching the server's data channel.
    session_key: Option<SessionKey>,
    /// Control messages decoded from a single datagram but not yet returned:
    /// one reliable frame can carry several, and [`recv`](Self::recv) hands them
    /// out one at a time.
    pending_control: VecDeque<ServerControl>,
    origin: Instant,
}

/// One thing that arrived on the client's socket, demultiplexed by packet type.
#[derive(Debug)]
pub enum Inbound {
    /// A decoded, authenticated control message.
    Control(ServerControl),
    /// A raw video packet (header included), for the reassembler to decode.
    Video(Vec<u8>),
    /// A raw audio packet (header included), for [`parse_audio_packet`](crate::parse_audio_packet)
    /// and the Opus decoder. Unauthenticated, like video.
    Audio(Vec<u8>),
    /// A decoded, authenticated rumble frame — motor levels (incl. the Xbox
    /// trigger motors) for a pad the client drives.
    Rumble(Rumble),
    /// A decoded, authenticated rich pad-output frame — motors, adaptive
    /// triggers, and LED for a pad the client drives.
    PadOutput(PadOutput),
    /// Anything the receiver ignores (an unknown type, or a frame that failed to
    /// authenticate or decode).
    Other,
}

impl ClientEndpoint {
    pub fn connect(server: SocketAddr, key: Option<SessionKey>) -> io::Result<ClientEndpoint> {
        let bind: SocketAddr = if server.is_ipv4() {
            "0.0.0.0:0".parse().expect("literal")
        } else {
            "[::]:0".parse().expect("literal")
        };
        let socket = UdpSocket::bind(bind)?;
        socket.set_read_timeout(Some(Duration::from_millis(500)))?;
        Ok(ClientEndpoint {
            socket,
            server,
            reliable: Reliable::new(MAX_CONTROL_PAYLOAD),
            key,
            secret: None,
            client_nonce: None,
            session_key: None,
            pending_control: VecDeque::new(),
            origin: Instant::now(),
        })
    }

    /// Connect with the raw pairing secret, so the client can complete the
    /// session-key handshake: control is signed with the pairing key, and once
    /// `SessionConfig` arrives (see [`recv_control`](Self::recv_control)) input
    /// switches to the derived session key.
    pub fn connect_paired(server: SocketAddr, secret: [u8; 32]) -> io::Result<ClientEndpoint> {
        let mut client = ClientEndpoint::connect(server, Some(SessionKey::from_bytes(secret)))?;
        client.secret = Some(secret);
        Ok(client)
    }

    /// Send `Hello`, remembering its nonce so the session key can be derived
    /// from the `SessionConfig` that answers it.
    pub fn send_hello(&mut self, hello: sunburst_core::proto::Hello) -> io::Result<()> {
        self.client_nonce = Some(hello.client_nonce);
        self.send_control(&ClientControl::Hello(hello))
    }

    /// The key outgoing input/NACK/feedback are signed with: the session key
    /// once installed, else the control key.
    fn data_key(&self) -> Option<&SessionKey> {
        self.session_key.as_ref().or(self.key.as_ref())
    }

    /// Send a NACK for `frame_id`. Empty `missing` abandons the frame.
    pub fn send_nack(&self, frame_id: Seq16, missing: &[u16]) -> io::Result<()> {
        let mut body = [0u8; MAX_PAYLOAD];
        let n = sunburst_core::proto::Nack::encode(missing, &mut body)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "too many NACK indices"))?;
        self.transmit_data(PacketType::Nack, frame_id, &body[..n])
    }

    /// Send a feedback report.
    pub fn send_feedback(&self, feedback: &Feedback) -> io::Result<()> {
        let mut body = [0u8; sunburst_core::proto::FEEDBACK_BODY_LEN];
        let n = feedback
            .encode(&mut body)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "feedback body"))?;
        self.transmit_data(PacketType::Feedback, Seq16(0), &body[..n])
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    fn now_ms(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }

    pub fn send_control(&mut self, message: &ClientControl) -> io::Result<()> {
        let encoded = message
            .encode()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        let now_ms = self.now_ms();
        let frame = self
            .reliable
            .send(&encoded, now_ms)
            .map_err(|e| io::Error::new(io::ErrorKind::WouldBlock, e.to_string()))?;
        self.transmit(PacketType::Control, &frame)
    }

    pub fn send_input(&mut self, packet: &InputPacket) -> io::Result<()> {
        let mut body = [0u8; sunburst_core::proto::input::MAX_INPUT_BODY];
        let n = packet
            .encode(&mut body)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "input body too large"))?;
        self.transmit_data(PacketType::Input, Seq16(0), &body[..n])
    }

    /// Send one pad-mic Opus frame (`AudioIn`, tagged with `pad_index`). Like
    /// the video/audio the server sends this way, it is **unauthenticated** —
    /// media rides the LAN without a MAC — so it carries no key; the server
    /// attributes it by source address, which is why it goes out this socket.
    /// `qpc` is the frame's capture-time tick counter, low 32 bits.
    pub fn send_audio_in(
        &self,
        pad_index: u8,
        seq: Seq16,
        qpc: u32,
        opus: &[u8],
    ) -> io::Result<()> {
        let mut buf = [0u8; crate::audio::MAX_AUDIO_PACKET];
        let n = crate::audio::encode_audio_in_packet(pad_index, seq, qpc, opus, &mut buf)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "audio-in body too large")
            })?;
        self.socket.send_to(&buf[..n], self.server)?;
        Ok(())
    }

    /// Send a control packet, signed with the control key.
    fn transmit(&self, packet_type: PacketType, body: &[u8]) -> io::Result<()> {
        self.emit(packet_type, Seq16(0), body, self.key.as_ref())
    }

    /// Send a data packet (input/NACK/feedback), signed with the data key.
    fn transmit_data(
        &self,
        packet_type: PacketType,
        frame_id: Seq16,
        body: &[u8],
    ) -> io::Result<()> {
        self.emit(packet_type, frame_id, body, self.data_key())
    }

    fn emit(
        &self,
        packet_type: PacketType,
        frame_id: Seq16,
        body: &[u8],
        key: Option<&SessionKey>,
    ) -> io::Result<()> {
        let header = Header {
            packet_type,
            flags: Flags::EMPTY,
            frame_id,
            qpc_timestamp: 0,
            pkt_idx: 0,
            pkt_count: 1,
        };
        let mut datagram = vec![0u8; HEADER_LEN];
        header.encode((&mut datagram[..]).try_into().expect("HEADER_LEN bytes"));
        datagram.extend_from_slice(body);
        if let Some(key) = key {
            key.sign_packet(&mut datagram);
        }
        self.socket.send_to(&datagram, self.server)?;
        Ok(())
    }

    /// Wait for one server message, if it arrives before the read timeout.
    pub fn recv_control(&mut self) -> io::Result<Option<ServerControl>> {
        let mut buf = [0u8; HEADER_LEN + MAX_PAYLOAD];
        let len = match self.socket.recv(&mut buf) {
            Ok(len) => len,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };

        let datagram = &buf[..len];
        let Some(header) = Header::decode(datagram) else {
            return Ok(None);
        };
        if header.packet_type != PacketType::Control {
            return Ok(None);
        }

        let mut body = &datagram[HEADER_LEN..];
        if let Some(key) = &self.key {
            match key.verify_packet(datagram) {
                Some(_) => body = &body[..body.len().saturating_sub(MAC_LEN)],
                None => return Ok(None),
            }
        }

        for message in self.reliable.on_frame(body) {
            if let Ok((decoded, _)) = ServerControl::decode(&message) {
                // The handshake's other half: derive the same session key the
                // server installed, so input switches to it from here on.
                if let ServerControl::SessionConfig(config) = &decoded
                    && let (Some(secret), Some(cn)) = (self.secret, self.client_nonce)
                {
                    self.session_key = Some(SessionKey::derive(&secret, &cn, &config.server_nonce));
                }
                return Ok(Some(decoded));
            }
        }
        Ok(None)
    }

    /// Receive one datagram and demultiplex it: a control message (verified and
    /// framed like [`recv_control`](Self::recv_control), switching the session
    /// key on `SessionConfig`), a raw video packet for the reassembler, or
    /// something the stub receiver ignores. `None` on the read timeout.
    ///
    /// Control frames can carry several messages; the extras are queued and
    /// returned by later calls before the socket is read again.
    pub fn recv(&mut self) -> io::Result<Option<Inbound>> {
        if let Some(msg) = self.pending_control.pop_front() {
            return Ok(Some(Inbound::Control(msg)));
        }
        let mut buf = [0u8; HEADER_LEN + MAX_PAYLOAD];
        let len = match self.socket.recv(&mut buf) {
            Ok(len) => len,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(None);
            }
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => return Ok(None),
            Err(e) => return Err(e),
        };
        let datagram = &buf[..len];
        let Some(header) = Header::decode(datagram) else {
            return Ok(Some(Inbound::Other));
        };

        match header.packet_type {
            PacketType::Video => Ok(Some(Inbound::Video(datagram.to_vec()))),
            // Audio is unauthenticated, like video: hand the raw datagram up for
            // the Opus decoder with no MAC to verify.
            PacketType::Audio => Ok(Some(Inbound::Audio(datagram.to_vec()))),
            PacketType::Control => {
                let mut body = &datagram[HEADER_LEN..];
                if let Some(key) = &self.key {
                    match key.verify_packet(datagram) {
                        Some(_) => body = &body[..body.len().saturating_sub(MAC_LEN)],
                        None => return Ok(Some(Inbound::Other)),
                    }
                }
                for message in self.reliable.on_frame(body) {
                    if let Ok((decoded, _)) = ServerControl::decode(&message) {
                        if let ServerControl::SessionConfig(config) = &decoded
                            && let (Some(secret), Some(cn)) = (self.secret, self.client_nonce)
                        {
                            self.session_key =
                                Some(SessionKey::derive(&secret, &cn, &config.server_nonce));
                        }
                        self.pending_control.push_back(decoded);
                    }
                }
                Ok(self.pending_control.pop_front().map(Inbound::Control))
            }
            // Rumble and pad output are authenticated (they drive hardware), so
            // verify the MAC before decoding — an unauthenticated one is dropped
            // as `Other`, never applied to a controller.
            PacketType::Rumble | PacketType::PadOutput => {
                // Signed with the data key (the session key once derived, like
                // the server's `session.data_key()`), not the control key.
                let Some(key) = self.data_key() else {
                    return Ok(Some(Inbound::Other));
                };
                let Some(verified) = key.verify_packet(datagram) else {
                    return Ok(Some(Inbound::Other));
                };
                let body = &verified[HEADER_LEN..];
                let inbound = match header.packet_type {
                    PacketType::Rumble => Rumble::decode(body).map(Inbound::Rumble),
                    _ => PadOutput::decode(body).map(Inbound::PadOutput),
                };
                Ok(Some(inbound.unwrap_or(Inbound::Other)))
            }
            _ => Ok(Some(Inbound::Other)),
        }
    }

    /// Emit retransmits and owed acks.
    pub fn tick(&mut self) -> io::Result<()> {
        let now_ms = self.now_ms();
        let frames = self
            .reliable
            .tick(now_ms)
            .map_err(|e| io::Error::new(io::ErrorKind::TimedOut, e.to_string()))?;
        for frame in frames {
            self.transmit(PacketType::Control, &frame)?;
        }
        Ok(())
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Nonce bytes, re-exported so callers do not need the pairing module directly.
pub const CLIENT_NONCE_LEN: usize = NONCE_LEN;

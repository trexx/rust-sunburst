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

use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sunburst_core::proto::padoutput::PAD_OUTPUT_MAX_BODY;
use sunburst_core::proto::pairing::NONCE_LEN;
use sunburst_core::proto::rumble::RUMBLE_BODY_LEN;
use sunburst_core::proto::{
    ClientControl, ClientMessage, Flags, HEADER_LEN, Header, InputPacket, MAC_LEN, MAX_PAYLOAD,
    PacketType, PadOutput, ReplayWindow, Rumble, Seq16, ServerControl, SessionKey,
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

/// An authenticated client. Survives a change of source address.
struct Session {
    key: SessionKey,
    addr: SocketAddr,
    reliable: Reliable,
    replay: ReplayWindow,
    last_seen_ms: u64,
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
        let socket = UdpSocket::bind(addr)?;
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
            // Video, audio, NACK, feedback and rumble have no handler until the
            // phases that produce them. Dropping is the honest response; a
            // placeholder would be a lie about what works.
            _ => {}
        }
    }

    /// Identify the sender, scanning the paired keys if the address is new.
    ///
    /// On success the session's return address is updated, so a client that
    /// reconnects from a new port keeps its replay window and its channel.
    fn authenticate(
        &mut self,
        datagram: &[u8],
        from: SocketAddr,
        hint: Option<u32>,
    ) -> Option<u32> {
        let now_ms = self.now_ms();

        if let Some(&client) = self.by_addr.get(&from)
            && let Some(session) = self.sessions.get_mut(&client)
            && session.key.verify_packet(datagram).is_some()
        {
            session.last_seen_ms = now_ms;
            return Some(client);
        }

        let mut keys = self.handler.client_keys();
        if let Some(want) = hint {
            // Only reorders the scan. A wrong id costs one extra verification.
            keys.sort_by_key(|(id, _)| *id != want);
        }

        for (client, key) in keys {
            if key.verify_packet(datagram).is_none() {
                continue;
            }

            match self.sessions.get_mut(&client) {
                Some(session) => {
                    // The same client from a different port. The replay window
                    // and the reliable channel come with it — that is the whole
                    // point of keying on the client.
                    self.by_addr.remove(&session.addr);
                    session.addr = from;
                    session.last_seen_ms = now_ms;
                    session.key = key;
                }
                None => {
                    self.sessions.insert(
                        client,
                        Session {
                            key,
                            addr: from,
                            reliable: Reliable::new(MAX_CONTROL_PAYLOAD),
                            replay: ReplayWindow::new(),
                            last_seen_ms: now_ms,
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
        if let Some(client) = self.authenticate(datagram, from, None) {
            let frame = &body[..body.len().saturating_sub(MAC_LEN)];
            let messages = {
                let session = self.sessions.get_mut(&client).expect("just authenticated");
                session.reliable.on_frame(frame)
            };
            for message in messages {
                self.on_authenticated(&message, client, now);
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

    fn on_authenticated(&mut self, message: &[u8], client: u32, _now: u64) {
        let Ok((decoded, _)) = ClientControl::decode(message) else {
            return;
        };

        match decoded {
            ClientControl::Hello(hello) => self.handler.on_hello(client, hello),
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
        let Some(client) = self.authenticate(datagram, from, None) else {
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

    fn forget(&mut self, client: u32) {
        if let Some(session) = self.sessions.remove(&client) {
            self.by_addr.remove(&session.addr);
        }
    }

    fn send_to_client(&mut self, client: u32, message: &ServerControl) {
        let Ok(encoded) = message.encode() else {
            return;
        };
        let now_ms = self.now_ms();

        let Some(session) = self.sessions.get_mut(&client) else {
            return;
        };
        let addr = session.addr;
        let key = session.key.clone();
        // A full window or an oversized message is a bug on this side, not
        // something the peer can fix by waiting.
        let Ok(frame) = session.reliable.send(&encoded, now_ms) else {
            return;
        };
        self.transmit(addr, &frame, Some(&key), PacketType::Control);
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
        self.transmit(session.addr, &body, Some(&session.key), PacketType::Rumble);
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
            Some(&session.key),
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
                        send.push((session.addr, frame, Some(session.key.clone())));
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
    pub key: Option<SessionKey>,
    origin: Instant,
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
            origin: Instant::now(),
        })
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
        self.transmit(PacketType::Input, &body[..n])
    }

    fn transmit(&self, packet_type: PacketType, body: &[u8]) -> io::Result<()> {
        let header = Header {
            packet_type,
            flags: Flags::EMPTY,
            frame_id: Seq16(0),
            qpc_timestamp: 0,
            pkt_idx: 0,
            pkt_count: 1,
        };
        let mut datagram = vec![0u8; HEADER_LEN];
        header.encode((&mut datagram[..]).try_into().expect("HEADER_LEN bytes"));
        datagram.extend_from_slice(body);
        if let Some(key) = &self.key {
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
                return Ok(Some(decoded));
            }
        }
        Ok(None)
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

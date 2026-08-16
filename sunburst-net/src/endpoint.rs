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
//! happens once per peer. A changed DHCP lease simply causes another scan.
//!
//! `Hello` carries a `client_id`, which is used only to order that scan. It is a
//! hint, not a credential: the MAC is what proves identity.
//!
//! # Pairing is the one unauthenticated path
//!
//! Before pairing there is no key, so `PairRequest` and `PairConfirm` arrive
//! without a MAC. They are processed only while the handler reports pairing
//! armed, only for those two kinds, and dropped silently otherwise.

use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sunburst_core::proto::pairing::NONCE_LEN;
use sunburst_core::proto::{
    ClientControl, ClientMessage, Flags, HEADER_LEN, Header, MAC_LEN, MAX_PAYLOAD, PacketType,
    ReplayWindow, Seq16, ServerControl, SessionKey,
};

use crate::handler::ControlHandler;
use crate::reliable::{FRAME_HEADER_LEN, Reliable, ReliableError};

/// Room for a control message once the common header, the reliable frame header
/// and the MAC are accounted for.
pub const MAX_CONTROL_PAYLOAD: usize = MAX_PAYLOAD - FRAME_HEADER_LEN - MAC_LEN;

/// How long a peer may go silent before its state is dropped.
const PEER_IDLE_SECS: u64 = 120;

/// Read timeout, which is also how often `tick` runs.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

struct Peer {
    reliable: Reliable,
    auth: Option<Authenticated>,
    last_seen_ms: u64,
}

struct Authenticated {
    client_id: u32,
    key: SessionKey,
    replay: ReplayWindow,
}

pub struct Endpoint<H: ControlHandler> {
    socket: UdpSocket,
    handler: H,
    peers: HashMap<SocketAddr, Peer>,
    origin: Instant,
}

impl<H: ControlHandler> Endpoint<H> {
    pub fn bind(addr: SocketAddr, handler: H) -> io::Result<Endpoint<H>> {
        let socket = UdpSocket::bind(addr)?;
        socket.set_read_timeout(Some(POLL_INTERVAL))?;
        Ok(Endpoint {
            socket,
            handler,
            peers: HashMap::new(),
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
    ///
    /// Split out from [`run`] so tests can step the endpoint deterministically
    /// rather than racing a thread.
    ///
    /// [`run`]: Endpoint::run
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
            // A refused datagram on a connectionless socket is normal on
            // Windows (ICMP port unreachable surfaces as ConnectionReset) and
            // says nothing about the socket's health.
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

    /// Find the key for this peer, scanning if necessary.
    ///
    /// `hint` orders the scan when `Hello` claimed an id. Returns whether the
    /// packet verified.
    fn authenticate(&mut self, datagram: &[u8], from: SocketAddr, hint: Option<u32>) -> bool {
        if let Some(auth) = self.peers.get(&from).and_then(|p| p.auth.as_ref())
            && auth.key.verify_packet(datagram).is_some()
        {
            return true;
        }

        let mut keys = self.handler.client_keys();
        if let Some(want) = hint {
            // Only reorders the scan. A wrong id costs one extra verification.
            keys.sort_by_key(|(id, _)| *id != want);
        }

        for (client_id, key) in keys {
            if key.verify_packet(datagram).is_some() {
                let now_ms = self.now_ms();
                let peer = self.peer_mut(from, now_ms);
                peer.auth = Some(Authenticated {
                    client_id,
                    key,
                    replay: ReplayWindow::new(),
                });
                return true;
            }
        }
        false
    }

    fn peer_mut(&mut self, from: SocketAddr, now_ms: u64) -> &mut Peer {
        self.peers.entry(from).or_insert_with(|| Peer {
            reliable: Reliable::new(MAX_CONTROL_PAYLOAD),
            auth: None,
            last_seen_ms: now_ms,
        })
    }

    fn on_control(&mut self, datagram: &[u8], body: &[u8], from: SocketAddr) {
        let now_ms = self.now_ms();
        let now = unix_now();

        // Authenticated first. Only if no key verifies is the unauthenticated
        // pairing path considered, and then only while armed.
        let authenticated = self.authenticate(datagram, from, None);

        let frame = if authenticated {
            // The MAC covers the whole packet; strip it before framing.
            &body[..body.len().saturating_sub(MAC_LEN)]
        } else {
            if !self.handler.pairing_armed(now) {
                return;
            }
            body
        };

        self.peer_mut(from, now_ms).last_seen_ms = now_ms;
        let messages = {
            let peer = self.peers.get_mut(&from).expect("just inserted");
            peer.reliable.on_frame(frame)
        };

        for message in messages {
            self.on_control_message(&message, from, authenticated, now);
        }
    }

    fn on_control_message(
        &mut self,
        message: &[u8],
        from: SocketAddr,
        authenticated: bool,
        now: u64,
    ) {
        let Ok((decoded, _)) = ClientControl::decode(message) else {
            return;
        };

        // The allow-list. An unauthenticated peer may only pair; anything else
        // arriving without a MAC is an attempt to skip authentication.
        if !authenticated {
            let allowed = decoded.kind().is_some_and(ClientMessage::is_pre_pairing);
            if !allowed {
                return;
            }
        }

        let client_id = self
            .peers
            .get(&from)
            .and_then(|p| p.auth.as_ref())
            .map(|a| a.client_id);

        match decoded {
            ClientControl::PairRequest(request) => {
                if let Some((request_id, server_nonce)) = self.handler.on_pair_request(request, now)
                {
                    self.send_control(
                        from,
                        &ServerControl::PairChallenge {
                            request_id,
                            server_nonce,
                        },
                        None,
                    );
                }
            }
            ClientControl::PairConfirm { request_id, tag } => {
                self.handler.on_pair_confirm(request_id, tag, now);
            }
            ClientControl::Hello(hello) => {
                if let Some(client) = client_id {
                    self.handler.on_hello(client, hello);
                }
            }
            ClientControl::ListApps => {
                let apps = self.handler.on_app_list();
                let key = self.key_for(from);
                self.send_control(from, &ServerControl::AppList(apps), key);
            }
            ClientControl::LaunchApp { app_id } => {
                let _ = self.handler.on_launch(app_id);
            }
            ClientControl::Bye => {
                if let Some(client) = client_id {
                    self.handler.on_bye(client);
                }
                self.peers.remove(&from);
            }
            // Reserved or not yet acted on. Decoding succeeded, so the stream is
            // intact; there is simply nothing to do until the phase that owns it.
            _ => {}
        }
    }

    fn on_input(&mut self, datagram: &[u8], body: &[u8], from: SocketAddr) {
        if !self.authenticate(datagram, from, None) {
            return;
        }
        let now_ms = self.now_ms();

        let Some(peer) = self.peers.get_mut(&from) else {
            return;
        };
        peer.last_seen_ms = now_ms;
        let Some(auth) = peer.auth.as_mut() else {
            return;
        };

        let payload = &body[..body.len().saturating_sub(MAC_LEN)];
        let Some(packet) = sunburst_core::proto::InputPacket::decode(payload) else {
            return;
        };

        // MAC verified above; freshness is this. Both halves are needed, since a
        // MAC is deterministic and a captured packet re-sent verbatim verifies.
        if !auth.replay.accept(packet.input_seq) {
            return;
        }
        let client_id = auth.client_id;
        self.handler.on_input(client_id, packet.event);
    }

    fn key_for(&self, from: SocketAddr) -> Option<SessionKey> {
        self.peers
            .get(&from)
            .and_then(|p| p.auth.as_ref())
            .map(|a| a.key.clone())
    }

    /// Frame, sign if there is a key, and transmit.
    fn send_control(&mut self, to: SocketAddr, message: &ServerControl, key: Option<SessionKey>) {
        let Ok(encoded) = message.encode() else {
            return;
        };
        let now_ms = self.now_ms();

        let frame = {
            let peer = self.peer_mut(to, now_ms);
            match peer.reliable.send(&encoded, now_ms) {
                Ok(frame) => frame,
                // A full window or an oversized message is a bug on this side,
                // not something the peer can fix by waiting.
                Err(_) => return,
            }
        };
        self.transmit(to, PacketType::Control, &frame, key.as_ref());
    }

    fn transmit(
        &self,
        to: SocketAddr,
        packet_type: PacketType,
        body: &[u8],
        key: Option<&SessionKey>,
    ) {
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
        if let Some(key) = key {
            key.sign_packet(&mut datagram);
        }
        let _ = self.socket.send_to(&datagram, to);
    }

    /// Retransmits, owed acks, and dropping peers that have gone quiet.
    fn tick(&mut self) {
        let now_ms = self.now_ms();
        let mut dead = Vec::new();
        let mut to_send: Vec<(SocketAddr, Vec<u8>, Option<SessionKey>)> = Vec::new();

        for (addr, peer) in &mut self.peers {
            if now_ms.saturating_sub(peer.last_seen_ms) > PEER_IDLE_SECS * 1000 {
                dead.push(*addr);
                continue;
            }
            match peer.reliable.tick(now_ms) {
                Ok(frames) => {
                    let key = peer.auth.as_ref().map(|a| a.key.clone());
                    for frame in frames {
                        to_send.push((*addr, frame, key.clone()));
                    }
                }
                Err(ReliableError::PeerGone) => dead.push(*addr),
                Err(_) => {}
            }
        }

        for (addr, frame, key) in to_send {
            self.transmit(addr, PacketType::Control, &frame, key.as_ref());
        }
        for addr in dead {
            self.peers.remove(&addr);
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

    pub fn send_input(&mut self, packet: &sunburst_core::proto::InputPacket) -> io::Result<()> {
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

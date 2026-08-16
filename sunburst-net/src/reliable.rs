// SPDX-License-Identifier: GPL-2.0-or-later

//! The reliable layer for control messages.
//!
//! Sequence, cumulative ack, retransmit on timeout. Not a general-purpose
//! stream: messages are small, infrequent, and there are never many outstanding.
//!
//! # No I/O
//!
//! [`Reliable::send`], [`Reliable::on_frame`] and [`Reliable::tick`] return
//! frames to transmit and messages to deliver rather than touching a socket.
//! That is what makes loss, reordering and duplication testable without one —
//! the same shape as `sunburst_web::api::dispatch` returning a response instead
//! of writing it.
//!
//! # Delivery is in order, deliberately
//!
//! The opposite choice from video. Pairing is a three-step exchange and a
//! `PairConfirm` overtaking its `PairRequest` is not something a handler should
//! have to reason about, so anything arriving early is held until its
//! predecessor lands.
//!
//! # Frame layout
//!
//! ```text
//! u16  seq      this frame's sequence, or the last one sent if payload is empty
//! u16  ack      highest contiguous sequence received from the peer
//! u8   flags    bit0: carries a payload
//! ...  payload  one control message, envelope included
//! ```
//!
//! Sequences are compared modularly through [`Seq16`], so the wrap at 65535 is
//! not a special case.

use std::collections::BTreeMap;

use sunburst_core::proto::Seq16;

/// Bytes of reliable framing before the control message.
pub const FRAME_HEADER_LEN: usize = 5;

/// Outstanding unacknowledged messages allowed.
///
/// Generous for traffic this infrequent; the window exists to bound memory
/// against a peer that stops acking, not to manage throughput.
pub const WINDOW: usize = 8;

/// How long to wait before resending.
pub const RETRANSMIT_MS: u64 = 200;

/// Retransmissions before the peer is declared gone.
///
/// Eight attempts at 200ms is about 1.6 seconds of silence on a LAN where the
/// round trip is under a millisecond.
pub const MAX_ATTEMPTS: u32 = 8;

const FLAG_HAS_PAYLOAD: u8 = 1;

#[derive(Debug, PartialEq, Eq)]
pub enum ReliableError {
    /// The send window is full; the caller should retry once something is acked.
    WouldBlock,
    /// Nothing has been acknowledged for [`MAX_ATTEMPTS`] retransmissions.
    PeerGone,
    TooLarge(usize),
}

impl core::fmt::Display for ReliableError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ReliableError::WouldBlock => write!(f, "the reliable send window is full"),
            ReliableError::PeerGone => write!(f, "the peer stopped acknowledging"),
            ReliableError::TooLarge(n) => write!(f, "a {n} byte control message exceeds the MTU"),
        }
    }
}

impl std::error::Error for ReliableError {}

struct Outstanding {
    frame: Vec<u8>,
    sent_at: u64,
    attempts: u32,
}

pub struct Reliable {
    /// Next sequence to hand out.
    next_seq: Seq16,
    /// Highest contiguous sequence received; what we advertise as our ack.
    ack: Option<Seq16>,
    /// Sent, not yet acknowledged.
    unacked: BTreeMap<u16, Outstanding>,
    /// Arrived out of order, held until the gap fills.
    early: BTreeMap<u16, Vec<u8>>,
    /// Set when a frame arrives, so the next outgoing frame carries an ack —
    /// or a bare one is emitted if there is nothing else to send.
    ack_pending: bool,
    max_payload: usize,
}

impl Reliable {
    /// `max_payload` is the room left for a control message after the common
    /// header, this frame header, and the MAC.
    pub fn new(max_payload: usize) -> Reliable {
        Reliable {
            next_seq: Seq16(0),
            ack: None,
            unacked: BTreeMap::new(),
            early: BTreeMap::new(),
            ack_pending: false,
            max_payload,
        }
    }

    /// Queue a message. Returns the frame to transmit.
    pub fn send(&mut self, message: &[u8], now_ms: u64) -> Result<Vec<u8>, ReliableError> {
        if message.len() > self.max_payload {
            return Err(ReliableError::TooLarge(message.len()));
        }
        if self.unacked.len() >= WINDOW {
            return Err(ReliableError::WouldBlock);
        }

        let seq = self.next_seq;
        self.next_seq = seq.next();

        let frame = self.build(seq, Some(message));
        self.unacked.insert(
            seq.0,
            Outstanding {
                frame: frame.clone(),
                sent_at: now_ms,
                attempts: 0,
            },
        );
        self.ack_pending = false;
        Ok(frame)
    }

    /// Take a frame from the peer. Returns messages ready to deliver, in order.
    pub fn on_frame(&mut self, frame: &[u8]) -> Vec<Vec<u8>> {
        if frame.len() < FRAME_HEADER_LEN {
            return Vec::new();
        }
        let seq = Seq16(u16::from_le_bytes([frame[0], frame[1]]));
        let peer_ack = Seq16(u16::from_le_bytes([frame[2], frame[3]]));
        let has_payload = frame[4] & FLAG_HAS_PAYLOAD != 0;

        // Retire everything the peer has acknowledged. Cumulative and modular,
        // so one ack clears a run and `is_newer_than` handles the wrap.
        self.unacked
            .retain(|&s, _| Seq16(s).is_newer_than(peer_ack));

        if !has_payload {
            return Vec::new();
        }

        self.ack_pending = true;

        // Already delivered, or a duplicate of something still held. Acking
        // again is the right response — the peer resent because our ack was
        // lost.
        if let Some(ack) = self.ack
            && !seq.is_newer_than(ack)
        {
            return Vec::new();
        }
        if self.early.contains_key(&seq.0) {
            return Vec::new();
        }

        // Refuse anything further ahead than the window. Without this a peer
        // could hold sequence 0 back and stream far-future ones, and the
        // holding area would grow without bound — the same reason the send
        // window exists, from the other direction.
        let want = self.ack.map_or(Seq16(0), Seq16::next);
        if seq.distance_from(want) >= WINDOW as i32 {
            return Vec::new();
        }

        self.early.insert(seq.0, frame[FRAME_HEADER_LEN..].to_vec());
        self.drain_in_order()
    }

    /// Frames to resend, and a bare ack if one is owed.
    pub fn tick(&mut self, now_ms: u64) -> Result<Vec<Vec<u8>>, ReliableError> {
        let mut out = Vec::new();

        for entry in self.unacked.values_mut() {
            if now_ms.saturating_sub(entry.sent_at) < RETRANSMIT_MS {
                continue;
            }
            entry.attempts += 1;
            if entry.attempts >= MAX_ATTEMPTS {
                return Err(ReliableError::PeerGone);
            }
            entry.sent_at = now_ms;
            // Rebuilt rather than resent verbatim, so the retransmission carries
            // our current ack. A stale ack would make the peer resend something
            // we already have.
            let seq = Seq16(u16::from_le_bytes([entry.frame[0], entry.frame[1]]));
            let payload = entry.frame[FRAME_HEADER_LEN..].to_vec();
            entry.frame = build_frame(seq, self.ack, Some(&payload));
            out.push(entry.frame.clone());
        }

        if self.ack_pending && out.is_empty() {
            self.ack_pending = false;
            out.push(self.build(self.next_seq, None));
        }
        Ok(out)
    }

    /// Whether anything is still in flight.
    pub fn is_idle(&self) -> bool {
        self.unacked.is_empty() && self.early.is_empty()
    }

    pub fn outstanding(&self) -> usize {
        self.unacked.len()
    }

    fn build(&self, seq: Seq16, payload: Option<&[u8]>) -> Vec<u8> {
        build_frame(seq, self.ack, payload)
    }

    /// Move everything now contiguous out of the holding area.
    fn drain_in_order(&mut self) -> Vec<Vec<u8>> {
        let mut ready = Vec::new();
        loop {
            let want = match self.ack {
                Some(ack) => ack.next(),
                // Nothing received yet: the first message must be sequence 0, so
                // a peer that starts mid-stream is held rather than delivered
                // out of order.
                None => Seq16(0),
            };
            let Some(payload) = self.early.remove(&want.0) else {
                break;
            };
            self.ack = Some(want);
            ready.push(payload);
        }
        ready
    }
}

fn build_frame(seq: Seq16, ack: Option<Seq16>, payload: Option<&[u8]>) -> Vec<u8> {
    let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + payload.map_or(0, <[u8]>::len));
    frame.extend_from_slice(&seq.0.to_le_bytes());
    // With nothing received, advertise one before zero. Modular comparison makes
    // that "I have nothing", and the peer's sequence 0 still reads as newer.
    frame.extend_from_slice(&ack.map_or(u16::MAX, |a| a.0).to_le_bytes());
    frame.push(if payload.is_some() {
        FLAG_HAS_PAYLOAD
    } else {
        0
    });
    if let Some(p) = payload {
        frame.extend_from_slice(p);
    }
    frame
}

#[cfg(test)]
mod tests;

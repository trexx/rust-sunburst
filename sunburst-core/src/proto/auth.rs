// SPDX-License-Identifier: GPL-2.0-or-later

//! Packet authentication.
//!
//! An unauthenticated UDP port that reaches `SendInput` is remote input
//! injection for anyone on the network, so input, control and rumble packets all
//! carry a MAC. Video and audio do not, per CLAUDE.md.
//!
//! # Why a MAC alone is not enough
//!
//! [`SessionKey::tag`] is a keyed PRF: authenticating, because the key is
//! secret, and deterministic, because every MAC is. Determinism is exactly why
//! freshness has to come from somewhere else — a captured packet re-sent
//! verbatim verifies perfectly. The tag proves origin and integrity;
//! [`super::seq::ReplayWindow`] proves the packet is new. Neither half is
//! optional, and verification does both in that order.
//!
//! # Why the key is per session
//!
//! Deriving the packet key straight from the pairing secret leaves a hole across
//! sessions rather than within one: `input_seq` restarts at zero, so a packet
//! captured yesterday verifies cleanly today and lands ahead of the replay
//! window. [`SessionKey::derive`] mixes in a nonce from each side, so yesterday's
//! capture fails the MAC instead.

use subtle::ConstantTimeEq;

/// Bytes of MAC appended to an authenticated packet.
pub const MAC_LEN: usize = 8;

/// Bytes of nonce each side contributes to the session key.
pub const NONCE_LEN: usize = 16;

/// Domain separator for the key derivation.
///
/// Versioned: if the derivation inputs ever change, this string changes with
/// them, and old and new peers then fail to talk rather than agreeing on a key
/// one of them computed differently.
const DERIVE_CONTEXT: &str = "sunburst 2026 session key v1";

/// A per-session packet authentication key.
///
/// Held in memory only, never written to disk.
#[derive(Clone)]
pub struct SessionKey([u8; 32]);

impl SessionKey {
    /// Derive from the pairing secret and both sides' handshake nonces.
    ///
    /// Both nonces are mixed in so neither end can force key reuse on its own:
    /// a client replaying an old `Hello` still faces a fresh `server_nonce`.
    pub fn derive(
        pairing_secret: &[u8],
        client_nonce: &[u8; NONCE_LEN],
        server_nonce: &[u8; NONCE_LEN],
    ) -> SessionKey {
        let mut hasher = blake3::Hasher::new_derive_key(DERIVE_CONTEXT);
        hasher.update(pairing_secret);
        hasher.update(client_nonce);
        hasher.update(server_nonce);
        SessionKey(*hasher.finalize().as_bytes())
    }

    /// Construct directly from key material. Tests and pairing only.
    pub fn from_bytes(key: [u8; 32]) -> SessionKey {
        SessionKey(key)
    }

    /// Tag for `message`, truncated to the `u64` the wire format specifies.
    ///
    /// 64 bits is sufficient here because forgery is online-only: an attacker
    /// must send packets and observe effects, so there is no offline search to
    /// accelerate and 2⁻⁶⁴ per attempt has no practical attack behind it.
    pub fn tag(&self, message: &[u8]) -> [u8; MAC_LEN] {
        let full = blake3::keyed_hash(&self.0, message);
        let mut out = [0u8; MAC_LEN];
        out.copy_from_slice(&full.as_bytes()[..MAC_LEN]);
        out
    }

    /// Constant-time tag check.
    ///
    /// Not `==`: a byte-at-a-time comparison leaks how much of a forged tag was
    /// correct, which turns 2⁻⁶⁴ into eight successive 2⁻⁸ problems.
    pub fn verify(&self, message: &[u8], tag: &[u8]) -> bool {
        if tag.len() != MAC_LEN {
            return false;
        }
        self.tag(message).ct_eq(tag).into()
    }

    /// Append a MAC over everything already in `packet`.
    ///
    /// The tag covers the whole preceding packet — header included — so a
    /// modified header is as detectable as a modified payload.
    pub fn sign_packet(&self, packet: &mut Vec<u8>) {
        let tag = self.tag(packet);
        packet.extend_from_slice(&tag);
    }

    /// Verify a signed packet and return its contents without the trailing MAC.
    pub fn verify_packet<'a>(&self, packet: &'a [u8]) -> Option<&'a [u8]> {
        if packet.len() < MAC_LEN {
            return None;
        }
        let (body, tag) = packet.split_at(packet.len() - MAC_LEN);
        if self.verify(body, tag) {
            Some(body)
        } else {
            None
        }
    }
}

// Keys must not reach a log or a panic message.
impl core::fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("SessionKey(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"pairing secret from the handshake";
    const CN: [u8; NONCE_LEN] = [1; NONCE_LEN];
    const SN: [u8; NONCE_LEN] = [2; NONCE_LEN];

    fn key() -> SessionKey {
        SessionKey::derive(SECRET, &CN, &SN)
    }

    #[test]
    fn a_tag_verifies_against_its_own_message() {
        let k = key();
        let msg = b"input packet bytes";
        assert!(k.verify(msg, &k.tag(msg)));
    }

    #[test]
    fn the_tag_is_deterministic_which_is_why_the_replay_window_exists() {
        // Stated as a test because the property is easy to mistake for a flaw.
        // It is not: it is why authentication alone cannot provide freshness.
        let k = key();
        assert_eq!(k.tag(b"same bytes"), k.tag(b"same bytes"));
    }

    #[test]
    fn any_change_to_the_message_fails() {
        let k = key();
        let tag = k.tag(b"input packet bytes");
        assert!(!k.verify(b"input packet bytev", &tag));
        assert!(!k.verify(b"input packet byte", &tag));
        assert!(!k.verify(b"", &tag));
    }

    #[test]
    fn a_different_session_cannot_verify() {
        // The whole point of per-session derivation: yesterday's captured packet
        // fails today, before the replay window is even consulted.
        let today = key();
        let tomorrow = SessionKey::derive(SECRET, &CN, &[3; NONCE_LEN]);
        let msg = b"captured yesterday";
        assert!(!tomorrow.verify(msg, &today.tag(msg)));

        // And a fresh client nonce is equally sufficient on its own.
        let other_client = SessionKey::derive(SECRET, &[9; NONCE_LEN], &SN);
        assert!(!other_client.verify(msg, &today.tag(msg)));
    }

    #[test]
    fn a_different_pairing_secret_cannot_verify() {
        let a = key();
        let b = SessionKey::derive(b"some other secret", &CN, &SN);
        let msg = b"hello";
        assert!(!b.verify(msg, &a.tag(msg)));
    }

    #[test]
    fn derivation_is_reproducible_on_both_ends() {
        // Client and server compute this independently and must agree.
        assert_eq!(key().tag(b"x"), key().tag(b"x"));
    }

    #[test]
    fn nonce_order_matters() {
        // Concatenating in the wrong order would still "work" between two peers
        // that agree, which is how an asymmetry becomes a compatibility bug.
        let forward = SessionKey::derive(SECRET, &CN, &SN);
        let reversed = SessionKey::derive(SECRET, &SN, &CN);
        assert_ne!(forward.tag(b"x"), reversed.tag(b"x"));
    }

    #[test]
    fn wrong_length_tags_are_refused() {
        let k = key();
        let tag = k.tag(b"msg");
        assert!(!k.verify(b"msg", &tag[..7]));
        assert!(!k.verify(b"msg", &[]));
    }

    #[test]
    fn packet_sign_and_verify_round_trip() {
        let k = key();
        let mut packet = b"header and payload".to_vec();
        let body_len = packet.len();
        k.sign_packet(&mut packet);
        assert_eq!(packet.len(), body_len + MAC_LEN);
        assert_eq!(k.verify_packet(&packet), Some(&b"header and payload"[..]));
    }

    #[test]
    fn a_tampered_packet_is_refused() {
        let k = key();
        let mut packet = b"header and payload".to_vec();
        k.sign_packet(&mut packet);

        // Flip a bit in the header, the place a length-only check would miss.
        let mut tampered = packet.clone();
        tampered[0] ^= 0x01;
        assert_eq!(k.verify_packet(&tampered), None);

        // And in the tag itself.
        let mut tampered = packet.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert_eq!(k.verify_packet(&tampered), None);
    }

    #[test]
    fn a_truncated_packet_is_refused_rather_than_panicking() {
        let k = key();
        assert_eq!(k.verify_packet(&[]), None);
        assert_eq!(k.verify_packet(&[0; MAC_LEN - 1]), None);
        // Exactly a MAC and nothing else is a well-formed empty body.
        let mut empty = Vec::new();
        k.sign_packet(&mut empty);
        assert_eq!(k.verify_packet(&empty), Some(&[][..]));
    }

    #[test]
    fn keys_do_not_appear_in_debug_output() {
        // A key reaching a log or a panic message would outlive the session it
        // was scoped to.
        let rendered = format!("{:?}", key());
        assert_eq!(rendered, "SessionKey(<redacted>)");
        assert!(!rendered.contains(char::is_numeric));
    }
}

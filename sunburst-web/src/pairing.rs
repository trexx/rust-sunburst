// SPDX-License-Identifier: GPL-2.0-or-later

//! Pairing: turning a PIN read off a TV into a long-lived shared secret.
//!
//! # The PIN never crosses the wire
//!
//! The client generates the PIN and displays it; the user types it into the web
//! UI. Both ends then derive the same secret independently:
//!
//! ```text
//! pairing_secret = BLAKE3::derive_key("sunburst pairing v1",
//!                                     pin ‖ client_nonce ‖ server_nonce)
//! ```
//!
//! Only the nonces are transmitted. The obvious alternative — server mints the
//! PIN, client sends it back — puts the PIN in a packet, where a passive
//! listener reads it directly and does not even have to guess. This direction
//! also happens to be the easy one to type: eight digits into a browser rather
//! than into a TV remote.
//!
//! # Proving the PIN was typed correctly
//!
//! The server never learns the real PIN, so it cannot check one directly. The
//! client sends a tag over its derived secret; the server derives a candidate
//! from whatever was typed and compares. A mismatch means the PIN was wrong,
//! and costs an attempt rather than the whole arming.
//!
//! # What this is not
//!
//! Not a key exchange. Anyone who captures a pairing exchange *and* one later
//! authenticated packet can grind the 8-digit PIN offline and recover a secret
//! that stays valid until revoked. That is a deliberate choice — see the
//! residual-risk note in the plan. The mitigations here are the free ones:
//! pairing is only possible while explicitly armed from the UI, the window is
//! short, the arming is single-use, and PIN attempts are capped.

use subtle::ConstantTimeEq;

use crate::client::{PairedClient, QuirksRecord};
use crate::random;

/// How long an arming stays open. Long enough to walk to the TV, short enough
/// that the window is not simply left open.
pub const WINDOW_SECS: u64 = 90;

/// PIN attempts per pending request before it is discarded.
///
/// The PIN is the only secret in this exchange, so unlimited retries against a
/// captured request would reduce it from 10^8 to however long someone is willing
/// to sit there.
pub const MAX_PIN_ATTEMPTS: u32 = 5;

const DERIVE_CONTEXT: &str = "sunburst pairing v1";
const CONFIRM_MESSAGE: &[u8] = b"sunburst pair confirm v1";

/// Bytes in the confirmation tag.
pub const TAG_LEN: usize = 8;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PairingError {
    /// Pairing was not armed, or the window closed.
    ///
    /// Returned rather than logged loudly: an unarmed pair request is the normal
    /// state of the world, not an incident.
    #[error("pairing is not armed")]
    NotArmed,
    #[error("no pending pair request with id {0}")]
    UnknownRequest(u32),
    #[error("the client has not sent its confirmation yet")]
    NotConfirmed,
    #[error("wrong PIN, {remaining} attempt(s) left")]
    WrongPin { remaining: u32 },
    #[error("too many wrong PINs; the request was discarded")]
    TooManyAttempts,
    #[error("{0}")]
    Random(String),
}

impl From<random::RandomError> for PairingError {
    fn from(e: random::RandomError) -> Self {
        PairingError::Random(e.to_string())
    }
}

/// A pair request that has arrived and is waiting for a PIN.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    pub id: u32,
    pub name: String,
    pub model: String,
    pub abi: String,
    pub quirks: QuirksRecord,
    pub client_nonce: [u8; 16],
    pub received_at: u64,
    /// Set when the client's `PairConfirm` arrives.
    pub tag: Option<[u8; TAG_LEN]>,
    pub attempts: u32,
}

/// What a client sends to begin pairing. No PIN, deliberately.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairRequest {
    pub name: String,
    pub model: String,
    pub abi: String,
    pub quirks: QuirksRecord,
    pub client_nonce: [u8; 16],
}

#[derive(Default)]
pub struct Pairing {
    armed: Option<Armed>,
    pending: Vec<Pending>,
    next_request_id: u32,
}

struct Armed {
    server_nonce: [u8; 16],
    expires_at: u64,
}

impl Pairing {
    pub fn new() -> Pairing {
        Pairing::default()
    }

    /// Open the pairing window. Returns when it closes.
    ///
    /// Re-arming replaces any previous arming and clears pending requests: the
    /// user pressing the button again means "start over", and leaving a stale
    /// half-finished request around would let it be confirmed later by accident.
    pub fn arm(&mut self, now: u64) -> Result<u64, PairingError> {
        let expires_at = now + WINDOW_SECS;
        self.armed = Some(Armed {
            server_nonce: random::nonce()?,
            expires_at,
        });
        self.pending.clear();
        Ok(expires_at)
    }

    pub fn disarm(&mut self) {
        self.armed = None;
        self.pending.clear();
    }

    pub fn is_armed(&self, now: u64) -> bool {
        self.armed.as_ref().is_some_and(|a| a.expires_at > now)
    }

    pub fn expires_at(&self, now: u64) -> Option<u64> {
        self.armed
            .as_ref()
            .filter(|a| a.expires_at > now)
            .map(|a| a.expires_at)
    }

    /// Accept a pair request. Returns its id and the server nonce to send back.
    pub fn receive_request(
        &mut self,
        request: PairRequest,
        now: u64,
    ) -> Result<(u32, [u8; 16]), PairingError> {
        self.drop_if_expired(now);
        let armed = self.armed.as_ref().ok_or(PairingError::NotArmed)?;

        let id = self.next_request_id;
        self.next_request_id += 1;
        self.pending.push(Pending {
            id,
            name: request.name,
            model: request.model,
            abi: request.abi,
            quirks: request.quirks,
            client_nonce: request.client_nonce,
            received_at: now,
            tag: None,
            attempts: 0,
        });
        Ok((id, armed.server_nonce))
    }

    /// Record the client's confirmation tag.
    pub fn receive_confirm(
        &mut self,
        request_id: u32,
        tag: [u8; TAG_LEN],
        now: u64,
    ) -> Result<(), PairingError> {
        self.drop_if_expired(now);
        if self.armed.is_none() {
            return Err(PairingError::NotArmed);
        }
        let pending = self
            .pending
            .iter_mut()
            .find(|p| p.id == request_id)
            .ok_or(PairingError::UnknownRequest(request_id))?;
        pending.tag = Some(tag);
        Ok(())
    }

    /// Requests waiting for a PIN, if the window is still open.
    pub fn pending(&mut self, now: u64) -> &[Pending] {
        self.drop_if_expired(now);
        &self.pending
    }

    /// Complete pairing with the PIN the user read off the TV.
    ///
    /// On success the window closes: an arming is good for exactly one device.
    pub fn confirm(
        &mut self,
        request_id: u32,
        pin: &str,
        client_id: u32,
        now: u64,
    ) -> Result<PairedClient, PairingError> {
        self.drop_if_expired(now);
        let armed = self.armed.as_ref().ok_or(PairingError::NotArmed)?;
        let server_nonce = armed.server_nonce;

        let index = self
            .pending
            .iter()
            .position(|p| p.id == request_id)
            .ok_or(PairingError::UnknownRequest(request_id))?;

        let expected_tag = self.pending[index].tag.ok_or(PairingError::NotConfirmed)?;
        let secret = derive_secret(pin, &self.pending[index].client_nonce, &server_nonce);

        if !tags_match(&confirm_tag(&secret), &expected_tag) {
            let pending = &mut self.pending[index];
            pending.attempts += 1;
            let remaining = MAX_PIN_ATTEMPTS.saturating_sub(pending.attempts);
            if remaining == 0 {
                self.pending.remove(index);
                return Err(PairingError::TooManyAttempts);
            }
            // The arming survives, so a typo costs a retype rather than a walk
            // back to the TV.
            return Err(PairingError::WrongPin { remaining });
        }

        let pending = self.pending.remove(index);
        self.armed = None;
        self.pending.clear();

        Ok(PairedClient {
            id: client_id,
            name: pending.name,
            model: pending.model,
            abi: pending.abi,
            quirks: pending.quirks,
            paired_at: now,
            last_seen: None,
            secret,
        })
    }

    fn drop_if_expired(&mut self, now: u64) {
        if self.armed.as_ref().is_some_and(|a| a.expires_at <= now) {
            self.armed = None;
            self.pending.clear();
        }
    }
}

/// Both ends compute this independently; only the nonces are ever transmitted.
pub fn derive_secret(pin: &str, client_nonce: &[u8; 16], server_nonce: &[u8; 16]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_derive_key(DERIVE_CONTEXT);
    hasher.update(pin.as_bytes());
    hasher.update(client_nonce);
    hasher.update(server_nonce);
    *hasher.finalize().as_bytes()
}

/// The client's proof that it derived the same secret.
pub fn confirm_tag(secret: &[u8; 32]) -> [u8; TAG_LEN] {
    let full = blake3::keyed_hash(secret, CONFIRM_MESSAGE);
    let mut tag = [0u8; TAG_LEN];
    tag.copy_from_slice(&full.as_bytes()[..TAG_LEN]);
    tag
}

fn tags_match(a: &[u8; TAG_LEN], b: &[u8; TAG_LEN]) -> bool {
    a.ct_eq(b).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000;

    fn request() -> PairRequest {
        PairRequest {
            name: "Living room".into(),
            model: "SHIELD Android TV".into(),
            abi: "arm64-v8a".into(),
            quirks: QuirksRecord::default(),
            client_nonce: [7; 16],
        }
    }

    /// Everything a real client does, given the PIN it chose to display.
    fn client_side(pin: &str, client_nonce: &[u8; 16], server_nonce: &[u8; 16]) -> [u8; TAG_LEN] {
        confirm_tag(&derive_secret(pin, client_nonce, server_nonce))
    }

    fn pair_successfully(p: &mut Pairing, pin: &str) -> PairedClient {
        p.arm(NOW).expect("arm");
        let (id, server_nonce) = p.receive_request(request(), NOW).expect("request");
        let tag = client_side(pin, &[7; 16], &server_nonce);
        p.receive_confirm(id, tag, NOW).expect("confirm");
        p.confirm(id, pin, 1, NOW).expect("pin")
    }

    #[test]
    fn a_correct_pin_produces_a_client_both_ends_agree_on() {
        let mut p = Pairing::new();
        p.arm(NOW).expect("arm");
        let (id, server_nonce) = p.receive_request(request(), NOW).expect("request");

        let pin = "12345678";
        let client_secret = derive_secret(pin, &[7; 16], &server_nonce);
        p.receive_confirm(id, confirm_tag(&client_secret), NOW)
            .expect("confirm");

        let paired = p.confirm(id, pin, 1, NOW).expect("pin accepted");
        assert_eq!(
            paired.secret, client_secret,
            "the two ends derived different secrets"
        );
        assert_eq!(paired.name, "Living room");
        assert_eq!(paired.paired_at, NOW);
    }

    #[test]
    fn an_unarmed_request_is_refused() {
        // The normal state of the world. Anything that arrives unsolicited is
        // dropped rather than queued for later approval.
        let mut p = Pairing::new();
        assert_eq!(
            p.receive_request(request(), NOW),
            Err(PairingError::NotArmed)
        );
        assert!(p.pending(NOW).is_empty());
    }

    #[test]
    fn an_expired_window_refuses_new_requests() {
        let mut p = Pairing::new();
        p.arm(NOW).expect("arm");
        assert!(p.is_armed(NOW + WINDOW_SECS - 1));
        assert!(!p.is_armed(NOW + WINDOW_SECS));
        assert_eq!(
            p.receive_request(request(), NOW + WINDOW_SECS),
            Err(PairingError::NotArmed)
        );
    }

    #[test]
    fn an_expired_window_discards_a_request_already_received() {
        // Otherwise a request could sit around and be confirmed long after the
        // user thought the window had shut.
        let mut p = Pairing::new();
        p.arm(NOW).expect("arm");
        let (id, nonce) = p.receive_request(request(), NOW).expect("request");
        p.receive_confirm(id, client_side("12345678", &[7; 16], &nonce), NOW)
            .expect("confirm");

        assert!(p.pending(NOW + WINDOW_SECS).is_empty());
        assert_eq!(
            p.confirm(id, "12345678", 1, NOW + WINDOW_SECS),
            Err(PairingError::NotArmed)
        );
    }

    #[test]
    fn a_wrong_pin_does_not_consume_the_arming() {
        let mut p = Pairing::new();
        p.arm(NOW).expect("arm");
        let (id, nonce) = p.receive_request(request(), NOW).expect("request");
        p.receive_confirm(id, client_side("12345678", &[7; 16], &nonce), NOW)
            .expect("confirm");

        assert_eq!(
            p.confirm(id, "87654321", 1, NOW),
            Err(PairingError::WrongPin { remaining: 4 })
        );
        assert!(p.is_armed(NOW), "a typo should not close the window");

        // And the right PIN still works afterwards.
        let paired = p.confirm(id, "12345678", 1, NOW).expect("retry");
        assert_eq!(paired.id, 1);
    }

    #[test]
    fn pin_attempts_are_capped() {
        // The PIN is the only secret here, so unlimited retries would reduce it
        // to however long someone is willing to sit and type.
        let mut p = Pairing::new();
        p.arm(NOW).expect("arm");
        let (id, nonce) = p.receive_request(request(), NOW).expect("request");
        p.receive_confirm(id, client_side("12345678", &[7; 16], &nonce), NOW)
            .expect("confirm");

        for attempt in 1..MAX_PIN_ATTEMPTS {
            assert_eq!(
                p.confirm(id, "00000000", 1, NOW),
                Err(PairingError::WrongPin {
                    remaining: MAX_PIN_ATTEMPTS - attempt
                })
            );
        }
        assert_eq!(
            p.confirm(id, "00000000", 1, NOW),
            Err(PairingError::TooManyAttempts)
        );
        // The request is gone, so even the correct PIN cannot revive it.
        assert_eq!(
            p.confirm(id, "12345678", 1, NOW),
            Err(PairingError::UnknownRequest(id))
        );
    }

    #[test]
    fn a_pin_works_exactly_once() {
        let mut p = Pairing::new();
        let paired = pair_successfully(&mut p, "12345678");
        assert_eq!(paired.id, 1);

        assert!(!p.is_armed(NOW), "success must close the window");
        assert_eq!(
            p.confirm(0, "12345678", 2, NOW),
            Err(PairingError::NotArmed)
        );
    }

    #[test]
    fn confirming_before_the_client_does_is_refused() {
        // Without the client's tag there is nothing to check the PIN against,
        // and storing an unverified secret would pair a device that can never
        // authenticate.
        let mut p = Pairing::new();
        p.arm(NOW).expect("arm");
        let (id, _) = p.receive_request(request(), NOW).expect("request");
        assert_eq!(
            p.confirm(id, "12345678", 1, NOW),
            Err(PairingError::NotConfirmed)
        );
    }

    #[test]
    fn re_arming_clears_a_half_finished_request() {
        let mut p = Pairing::new();
        p.arm(NOW).expect("arm");
        p.receive_request(request(), NOW).expect("request");
        assert_eq!(p.pending(NOW).len(), 1);

        p.arm(NOW).expect("re-arm");
        assert!(
            p.pending(NOW).is_empty(),
            "starting over must not leave the old request confirmable"
        );
    }

    #[test]
    fn re_arming_changes_the_server_nonce() {
        // Otherwise a captured exchange could be replayed against a later
        // arming, and the derived secret would be the same one.
        let mut p = Pairing::new();
        p.arm(NOW).expect("arm");
        let (_, first) = p.receive_request(request(), NOW).expect("request");
        p.arm(NOW).expect("re-arm");
        let (_, second) = p.receive_request(request(), NOW).expect("request");
        assert_ne!(first, second);
    }

    #[test]
    fn two_devices_pairing_at_once_are_kept_apart() {
        let mut p = Pairing::new();
        p.arm(NOW).expect("arm");

        let mut shield = request();
        shield.name = "Shield".into();
        shield.client_nonce = [1; 16];
        let (shield_id, nonce) = p.receive_request(shield, NOW).expect("request");

        let mut homatics = request();
        homatics.name = "Homatics".into();
        homatics.client_nonce = [2; 16];
        let (homatics_id, _) = p.receive_request(homatics, NOW).expect("request");
        assert_ne!(shield_id, homatics_id);

        p.receive_confirm(shield_id, client_side("11111111", &[1; 16], &nonce), NOW)
            .expect("confirm");
        p.receive_confirm(homatics_id, client_side("22222222", &[2; 16], &nonce), NOW)
            .expect("confirm");

        // The Shield's PIN must not pair the Homatics.
        assert!(matches!(
            p.confirm(homatics_id, "11111111", 1, NOW),
            Err(PairingError::WrongPin { .. })
        ));
        let paired = p.confirm(homatics_id, "22222222", 1, NOW).expect("pin");
        assert_eq!(paired.name, "Homatics");
    }

    #[test]
    fn different_pins_derive_different_secrets() {
        let a = derive_secret("12345678", &[1; 16], &[2; 16]);
        let b = derive_secret("12345679", &[1; 16], &[2; 16]);
        assert_ne!(a, b);
    }

    #[test]
    fn different_nonces_derive_different_secrets() {
        let pin = "12345678";
        assert_ne!(
            derive_secret(pin, &[1; 16], &[2; 16]),
            derive_secret(pin, &[9; 16], &[2; 16])
        );
        assert_ne!(
            derive_secret(pin, &[1; 16], &[2; 16]),
            derive_secret(pin, &[1; 16], &[9; 16])
        );
    }

    #[test]
    fn nonce_order_matters_in_the_derivation() {
        // Concatenating in the wrong order still "works" between two ends that
        // agree, which is how an asymmetry becomes a compatibility bug rather
        // than a test failure.
        let pin = "12345678";
        assert_ne!(
            derive_secret(pin, &[1; 16], &[2; 16]),
            derive_secret(pin, &[2; 16], &[1; 16])
        );
    }

    #[test]
    fn an_unknown_request_id_is_refused() {
        let mut p = Pairing::new();
        p.arm(NOW).expect("arm");
        assert_eq!(
            p.confirm(99, "12345678", 1, NOW),
            Err(PairingError::UnknownRequest(99))
        );
        assert_eq!(
            p.receive_confirm(99, [0; TAG_LEN], NOW),
            Err(PairingError::UnknownRequest(99))
        );
    }
}

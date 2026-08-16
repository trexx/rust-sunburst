// SPDX-License-Identifier: GPL-2.0-or-later

//! Pairing derivation — the half both ends of the link compute.
//!
//! Lives in core rather than in the server, because a client has to derive the
//! identical secret from the identical inputs. Putting it in `sunburst-web`
//! would mean every client dragging in `hyper` to compute two hashes.
//!
//! The *state machine* — arming, pending requests, attempt counting — is not
//! here. Only a server arms anything, so that stays in `sunburst-web::pairing`.
//!
//! # The PIN never crosses the wire
//!
//! The client generates the PIN and displays it; the user types it into the web
//! UI. Both ends then derive the same secret from it independently, and only the
//! nonces are transmitted. The obvious alternative — server mints the PIN,
//! client sends it back — puts the PIN in a packet, where a passive listener
//! reads it directly and does not have to guess at all.
//!
//! # What this is not
//!
//! Not a key exchange. Anyone who captures a pairing exchange *and* one later
//! authenticated packet can grind the eight-digit PIN offline and recover a
//! secret that stays valid until revoked. That is a deliberate choice for a
//! LAN-only threat model — see PROTOCOL.md — and the mitigations live with the
//! state machine: armed only from the UI, single use, short window, capped
//! attempts.

use subtle::ConstantTimeEq;

/// Bytes in the confirmation tag.
pub const TAG_LEN: usize = 8;

/// Bytes of nonce each side contributes.
pub const NONCE_LEN: usize = 16;

/// Digits in a pairing PIN.
///
/// Eight rather than four. It is the same effort to read off a TV and type into
/// a browser, and it is four orders of magnitude on the one number an offline
/// attack gets to grind.
pub const PIN_DIGITS: usize = 8;

/// Domain separators. Versioned: if the inputs ever change, these change with
/// them, so old and new peers fail to agree rather than agreeing on a key one of
/// them computed differently.
const DERIVE_CONTEXT: &str = "sunburst pairing v1";
const CONFIRM_MESSAGE: &[u8] = b"sunburst pair confirm v1";

/// The long-lived secret. Both ends compute this independently.
///
/// Nonce order is part of the contract: swapping them still "works" between two
/// ends that agree, which is how an asymmetry becomes a compatibility bug rather
/// than a test failure.
pub fn derive_secret(
    pin: &str,
    client_nonce: &[u8; NONCE_LEN],
    server_nonce: &[u8; NONCE_LEN],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_derive_key(DERIVE_CONTEXT);
    hasher.update(pin.as_bytes());
    hasher.update(client_nonce);
    hasher.update(server_nonce);
    *hasher.finalize().as_bytes()
}

/// The client's proof that it derived the same secret.
///
/// The server never learns the real PIN, so it cannot check one directly. It
/// derives a candidate from whatever was typed and compares tags.
pub fn confirm_tag(secret: &[u8; 32]) -> [u8; TAG_LEN] {
    let full = blake3::keyed_hash(secret, CONFIRM_MESSAGE);
    let mut tag = [0u8; TAG_LEN];
    tag.copy_from_slice(&full.as_bytes()[..TAG_LEN]);
    tag
}

/// Constant-time tag comparison.
pub fn tags_match(a: &[u8; TAG_LEN], b: &[u8; TAG_LEN]) -> bool {
    a.ct_eq(b).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_ends_agree_given_the_same_inputs() {
        let pin = "12345678";
        let client = [7; NONCE_LEN];
        let server = [9; NONCE_LEN];

        let secret = derive_secret(pin, &client, &server);
        assert!(tags_match(
            &confirm_tag(&secret),
            &confirm_tag(&derive_secret(pin, &client, &server))
        ));
    }

    #[test]
    fn a_different_pin_derives_a_different_secret() {
        assert_ne!(
            derive_secret("12345678", &[1; NONCE_LEN], &[2; NONCE_LEN]),
            derive_secret("12345679", &[1; NONCE_LEN], &[2; NONCE_LEN])
        );
    }

    #[test]
    fn a_different_nonce_on_either_side_derives_a_different_secret() {
        let pin = "12345678";
        assert_ne!(
            derive_secret(pin, &[1; NONCE_LEN], &[2; NONCE_LEN]),
            derive_secret(pin, &[9; NONCE_LEN], &[2; NONCE_LEN])
        );
        assert_ne!(
            derive_secret(pin, &[1; NONCE_LEN], &[2; NONCE_LEN]),
            derive_secret(pin, &[1; NONCE_LEN], &[9; NONCE_LEN])
        );
    }

    #[test]
    fn nonce_order_matters() {
        let pin = "12345678";
        assert_ne!(
            derive_secret(pin, &[1; NONCE_LEN], &[2; NONCE_LEN]),
            derive_secret(pin, &[2; NONCE_LEN], &[1; NONCE_LEN])
        );
    }

    #[test]
    fn a_wrong_pin_produces_a_tag_that_does_not_match() {
        let client = [7; NONCE_LEN];
        let server = [9; NONCE_LEN];
        let right = confirm_tag(&derive_secret("12345678", &client, &server));
        let wrong = confirm_tag(&derive_secret("87654321", &client, &server));
        assert!(!tags_match(&right, &wrong));
    }

    #[test]
    fn the_tag_is_a_truncation_not_the_whole_hash() {
        // Eight bytes, so a forgery is 2^-64 per attempt against an online-only
        // check. The full hash would be wasted bytes on the wire.
        let tag = confirm_tag(&[0xAB; 32]);
        assert_eq!(tag.len(), TAG_LEN);
        assert_eq!(
            &tag[..],
            &blake3::keyed_hash(&[0xAB; 32], CONFIRM_MESSAGE).as_bytes()[..TAG_LEN]
        );
    }

    #[test]
    fn the_derivation_is_domain_separated_from_the_session_key() {
        // Both use BLAKE3 derive_key. If they shared a context, a pairing secret
        // and a session key derived from the same bytes would collide.
        let material = [0u8; NONCE_LEN];
        let pairing = derive_secret("12345678", &material, &material);
        let session = blake3::Hasher::new_derive_key("sunburst 2026 session key v1")
            .update(b"12345678")
            .update(&material)
            .update(&material)
            .finalize();
        assert_ne!(&pairing, session.as_bytes());
    }
}

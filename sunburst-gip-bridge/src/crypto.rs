// SPDX-License-Identifier: GPL-2.0-or-later

//! The GIP security-handshake crypto, in Rust.
//!
//! The vendored xow driver's `utils/crypto.cpp` delegates these five primitives
//! to a Java `GipCrypto` class (Android's providers, mbedtls) over JNI, because
//! doing them natively there would mean enabling bignum/RSA/EC in the mbedtls
//! build. Sunburst has neither that Java class nor mbedtls, so the primitives are
//! reimplemented here with RustCrypto — pure Rust, cross-compiling to both ABIs,
//! and host-testable, which also makes the handshake the first piece of the
//! "Rustify the GIP logic" follow-up done up front.
//!
//! The byte formats are pinned to the reference `GipCrypto.java`:
//! - `rsa_pkcs1v15_encrypt` takes a **PKCS#1 `RSAPublicKey`** DER (a SEQUENCE of
//!   modulus and exponent, lifted from the controller's certificate — *not* a
//!   SubjectPublicKeyInfo) and RSA/PKCS#1 v1.5 padding.
//! - `ecdh_p256` takes the peer key as raw affine `X‖Y` (64 bytes, no `0x04`
//!   prefix), agrees over NIST P-256, and returns 96 bytes: our public key as
//!   raw `X‖Y` (64), then `SHA-256` of the agreed secret — the X coordinate alone
//!   (32).
//!
//! The `sb_crypto_*` C exports at the bottom are the seam `crypto.cpp` calls once
//! the C++ is vendored (B1c); everything above them is safe and host-tested.

use hmac::{Hmac, Mac};
use p256::PublicKey;
use p256::ecdh::EphemeralSecret;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use rand_core::{OsRng, RngCore};
use rsa::pkcs1::DecodeRsaPublicKey;
use rsa::{Pkcs1v15Encrypt, RsaPublicKey};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// The P-256 raw public-key length (`X‖Y`, no prefix), and the ECDH output's
/// public-key half.
const P256_RAW_LEN: usize = 64;
/// `ecdh_p256` output: our raw public key then `SHA-256` of the shared secret.
const ECDH_OUT_LEN: usize = P256_RAW_LEN + 32;

/// `SHA-256` of `data`.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

/// `HMAC-SHA256` of `data` under `key`. HMAC accepts a key of any length, so this
/// cannot fail.
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// Fill `out` with cryptographically-random bytes from the OS CSPRNG.
pub fn fill_random(out: &mut [u8]) {
    OsRng.fill_bytes(out);
}

/// Encrypt `data` under an RSA public key (PKCS#1 `RSAPublicKey` DER) with PKCS#1
/// v1.5 padding. `None` if the key or input is refused. The ciphertext is the key
/// size — 256 bytes for the handshake's 2048-bit key.
pub fn rsa_pkcs1v15_encrypt(public_key_der: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    let key = RsaPublicKey::from_pkcs1_der(public_key_der).ok()?;
    key.encrypt(&mut OsRng, Pkcs1v15Encrypt, data).ok()
}

/// The v2 handshake's ECDH over NIST P-256. Generates an ephemeral key pair,
/// agrees with the device's `peer` public key (raw `X‖Y`, 64 bytes), and returns
/// 96 bytes: our public key as raw `X‖Y` (64), then `SHA-256` of the agreed
/// secret (the X coordinate). `None` if the peer key is not a valid point.
pub fn ecdh_p256(peer: &[u8; P256_RAW_LEN]) -> Option<[u8; ECDH_OUT_LEN]> {
    // The protocol carries bare points; SEC1 wants the uncompressed `0x04` tag.
    let mut sec1 = [0u8; 1 + P256_RAW_LEN];
    sec1[0] = 0x04;
    sec1[1..].copy_from_slice(peer);
    let peer_key = PublicKey::from_sec1_bytes(&sec1).ok()?;

    let secret = EphemeralSecret::random(&mut OsRng);
    // Uncompressed `0x04‖X‖Y`; strip the tag to the raw form the protocol wants.
    let our_point = secret.public_key().to_encoded_point(false);
    let our_bytes = our_point.as_bytes();
    if our_bytes.len() != 1 + P256_RAW_LEN {
        return None;
    }

    let shared = secret.diffie_hellman(&peer_key);
    let hashed = sha256(&shared.raw_secret_bytes()[..]);

    let mut out = [0u8; ECDH_OUT_LEN];
    out[..P256_RAW_LEN].copy_from_slice(&our_bytes[1..]);
    out[P256_RAW_LEN..].copy_from_slice(&hashed);
    Some(out)
}

// ---------------------------------------------------------------- C FFI seam
//
// What `vendor/shim`'s `crypto.cpp` calls once the driver is vendored (B1c),
// replacing the JNI hop to Java. Exported unconditionally so host tests and the
// C++ both link the same symbols. Each writes into a caller-owned buffer.

/// # Safety
/// `data` must point to `len` readable bytes and `out32` to 32 writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sb_crypto_sha256(data: *const u8, len: usize, out32: *mut u8) {
    // SAFETY: the caller guarantees the pointer/length invariants above.
    let input = unsafe { slice(data, len) };
    let digest = sha256(input);
    // SAFETY: `out32` is 32 writable bytes per the contract.
    unsafe { write_out(out32, &digest) };
}

/// # Safety
/// `key`/`data` must point to `klen`/`dlen` readable bytes, `out32` to 32 writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sb_crypto_hmac_sha256(
    key: *const u8,
    klen: usize,
    data: *const u8,
    dlen: usize,
    out32: *mut u8,
) {
    // SAFETY: pointer/length invariants are the caller's per the contract.
    let (k, d) = unsafe { (slice(key, klen), slice(data, dlen)) };
    let mac = hmac_sha256(k, d);
    // SAFETY: `out32` is 32 writable bytes.
    unsafe { write_out(out32, &mac) };
}

/// # Safety
/// `out` must point to `len` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sb_crypto_random(out: *mut u8, len: usize) {
    if out.is_null() || len == 0 {
        return;
    }
    // SAFETY: `out` is `len` writable bytes per the contract.
    let buf = unsafe { std::slice::from_raw_parts_mut(out, len) };
    fill_random(buf);
}

/// Encrypt into `out` (capacity `out_cap`); returns the ciphertext length, or 0
/// on failure or if it would not fit.
///
/// # Safety
/// `public_key`/`data` must point to `klen`/`dlen` readable bytes, `out` to
/// `out_cap` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sb_crypto_rsa_pkcs1v15_encrypt(
    public_key: *const u8,
    klen: usize,
    data: *const u8,
    dlen: usize,
    out: *mut u8,
    out_cap: usize,
) -> usize {
    // SAFETY: pointer/length invariants are the caller's per the contract.
    let (key, msg) = unsafe { (slice(public_key, klen), slice(data, dlen)) };
    let Some(ct) = rsa_pkcs1v15_encrypt(key, msg) else {
        return 0;
    };
    if ct.len() > out_cap || out.is_null() {
        return 0;
    }
    // SAFETY: `ct.len() <= out_cap` writable bytes at `out`.
    unsafe { std::ptr::copy_nonoverlapping(ct.as_ptr(), out, ct.len()) };
    ct.len()
}

/// # Safety
/// `peer64` must point to 64 readable bytes and `out96` to 96 writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sb_crypto_ecdh_p256(peer64: *const u8, out96: *mut u8) -> bool {
    if peer64.is_null() || out96.is_null() {
        return false;
    }
    // SAFETY: `peer64` is 64 readable bytes per the contract.
    let peer = unsafe { slice(peer64, P256_RAW_LEN) };
    let peer: &[u8; P256_RAW_LEN] = peer.try_into().expect("exactly 64 bytes");
    let Some(result) = ecdh_p256(peer) else {
        return false;
    };
    // SAFETY: `out96` is 96 writable bytes.
    unsafe { write_out(out96, &result) };
    true
}

/// Borrow `len` bytes at `ptr` (empty for a null/zero-length pair).
///
/// # Safety
/// `ptr` must point to `len` readable bytes, or be null with `len == 0`.
unsafe fn slice<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if ptr.is_null() || len == 0 {
        &[]
    } else {
        // SAFETY: caller guarantees `len` readable bytes at `ptr`.
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }
}

/// Copy `src` to `dst`.
///
/// # Safety
/// `dst` must point to at least `src.len()` writable bytes.
unsafe fn write_out(dst: *mut u8, src: &[u8]) {
    if !dst.is_null() {
        // SAFETY: caller guarantees `src.len()` writable bytes at `dst`.
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Decode a hex string to bytes for the known-answer vectors.
    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }

    #[test]
    fn sha256_matches_the_nist_abc_vector() {
        // FIPS 180-2 / the classic "abc" vector.
        assert_eq!(
            sha256(b"abc"),
            hex("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad").as_slice(),
        );
        // The empty-input vector, to exercise the null/zero path too.
        assert_eq!(
            sha256(b""),
            hex("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855").as_slice(),
        );
    }

    #[test]
    fn hmac_sha256_matches_rfc_4231_case_2() {
        // RFC 4231 test case 2: key "Jefe", data "what do ya want for nothing?".
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            mac,
            hex("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843").as_slice(),
        );
    }

    #[test]
    fn random_fills_the_buffer_and_varies() {
        let mut a = [0u8; 48];
        let mut b = [0u8; 48];
        fill_random(&mut a);
        fill_random(&mut b);
        // Astronomically unlikely to be all-zero or equal; catches a no-op RNG.
        assert_ne!(a, [0u8; 48]);
        assert_ne!(a, b);
    }

    #[test]
    fn rsa_pkcs1v15_encrypt_round_trips_through_a_generated_key() {
        use rsa::RsaPrivateKey;
        use rsa::pkcs1::EncodeRsaPublicKey;

        // Generate a key, hand our function the public half as PKCS#1 DER (the
        // shape the handshake lifts from the certificate), and decrypt with the
        // private half to prove padding + parsing match.
        let private = RsaPrivateKey::new(&mut OsRng, 2048).expect("keygen");
        let public = private.to_public_key();
        let der = public.to_pkcs1_der().expect("pkcs1 der");

        let msg = b"gip pre-master secret";
        let ct = rsa_pkcs1v15_encrypt(der.as_bytes(), msg).expect("encrypt");
        assert_eq!(ct.len(), 256, "2048-bit key => 256-byte ciphertext");

        let recovered = private.decrypt(Pkcs1v15Encrypt, &ct).expect("decrypt");
        assert_eq!(recovered, msg);
    }

    #[test]
    fn rsa_rejects_a_non_pkcs1_key() {
        assert!(rsa_pkcs1v15_encrypt(b"not a der key", b"x").is_none());
    }

    #[test]
    fn ecdh_p256_agrees_between_two_parties() {
        // Stand up a second party with the same primitive and check both sides
        // derive the same shared-secret hash — the property the handshake needs.
        let alice_secret = EphemeralSecret::random(&mut OsRng);
        let alice_point = alice_secret.public_key().to_encoded_point(false);
        let alice_raw: [u8; 64] = alice_point.as_bytes()[1..].try_into().expect("64");

        // Our function plays Bob: agrees with Alice's key, returns Bob's pub + hash.
        let out = ecdh_p256(&alice_raw).expect("ecdh");
        let bob_raw: [u8; 64] = out[..64].try_into().unwrap();
        let bob_hash = &out[64..];

        // Alice completes the agreement against Bob's returned public key.
        let mut bob_sec1 = [0u8; 65];
        bob_sec1[0] = 0x04;
        bob_sec1[1..].copy_from_slice(&bob_raw);
        let bob_pub = PublicKey::from_sec1_bytes(&bob_sec1).expect("bob pub");
        let alice_shared = alice_secret.diffie_hellman(&bob_pub);
        let alice_hash = sha256(&alice_shared.raw_secret_bytes()[..]);

        assert_eq!(
            bob_hash, alice_hash,
            "both sides must derive the same secret"
        );
    }

    #[test]
    fn ecdh_rejects_a_point_not_on_the_curve() {
        // All-0xFF is not a valid P-256 point.
        assert!(ecdh_p256(&[0xFF; 64]).is_none());
    }
}

/*
 * Android port addition - not part of upstream xow.
 *
 * This program is free software; you can redistribute it and/or
 * modify it under the terms of the GNU General Public License
 * as published by the Free Software Foundation; either version 2
 * of the License, or (at your option) any later version.
 */

#pragma once

#include <cstddef>
#include <cstdint>
#include <vector>

/*
 * The GIP security handshake's crypto.
 *
 * Sunburst: upstream performed these in Java (Android's providers). This port
 * forwards them to Rust (RustCrypto) through the `sb_crypto_*` FFI — see
 * `src/crypto.rs` and `vendor/shim/crypto.cpp`. Same byte formats, no JNI, no
 * mbedtls, and host-tested. Nothing here is on a per-frame or per-report path.
 */
namespace GipCrypto
{
    /* All return an empty vector on failure. */
    std::vector<uint8_t> sha256(const uint8_t *data, size_t length);
    std::vector<uint8_t> hmacSha256(const uint8_t *key, size_t keyLength,
                                    const uint8_t *data, size_t length);
    std::vector<uint8_t> randomBytes(size_t count);

    /* Encrypts with PKCS#1 v1.5 under a DER RSAPublicKey. */
    std::vector<uint8_t> rsaEncrypt(const uint8_t *publicKey, size_t keyLength,
                                    const uint8_t *data, size_t length);

    /*
     * The v2 handshake's ECDH over NIST P-256, generating an ephemeral key pair and agreeing with
     * the device's key in one step - the private half is never needed again.
     *
     * Keys are raw affine X||Y, 64 bytes, with no uncompressed-point prefix.
     *
     * @return 96 bytes: our public key, then the SHA-256 of the agreed secret, which is what the
     *         PRF consumes. Empty on failure.
     */
    std::vector<uint8_t> ecdhP256(const uint8_t *peerKey, size_t length);
}

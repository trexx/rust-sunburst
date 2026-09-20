// SPDX-License-Identifier: GPL-2.0-or-later
//
// Sunburst: the GIP handshake's `GipCrypto`, forwarded to the Rust `sb_crypto_*`
// FFI (src/crypto.rs), replacing xow's Java/mbedtls implementation. Same byte
// formats, host-tested on the Rust side. This file stands in for the vendored
// utils/crypto.cpp, which is not copied.

#include "../xow/utils/crypto.h"

#include <cstddef>
#include <cstdint>

extern "C" {
void sb_crypto_sha256(const uint8_t *data, size_t len, uint8_t *out32);
void sb_crypto_hmac_sha256(const uint8_t *key, size_t klen, const uint8_t *data, size_t dlen,
                           uint8_t *out32);
void sb_crypto_random(uint8_t *out, size_t len);
size_t sb_crypto_rsa_pkcs1v15_encrypt(const uint8_t *pub, size_t klen, const uint8_t *data,
                                      size_t dlen, uint8_t *out, size_t out_cap);
bool sb_crypto_ecdh_p256(const uint8_t *peer64, uint8_t *out96);
}

namespace GipCrypto {

std::vector<uint8_t> sha256(const uint8_t *data, size_t length) {
    std::vector<uint8_t> out(32);
    sb_crypto_sha256(data, length, out.data());
    return out;
}

std::vector<uint8_t> hmacSha256(const uint8_t *key, size_t keyLength, const uint8_t *data,
                                size_t length) {
    std::vector<uint8_t> out(32);
    sb_crypto_hmac_sha256(key, keyLength, data, length, out.data());
    return out;
}

std::vector<uint8_t> randomBytes(size_t count) {
    std::vector<uint8_t> out(count);
    sb_crypto_random(out.data(), count);
    return out;
}

std::vector<uint8_t> rsaEncrypt(const uint8_t *publicKey, size_t keyLength, const uint8_t *data,
                                size_t length) {
    // The ciphertext is the key size; 512 bytes covers up to RSA-4096, well past
    // the handshake's 2048-bit key.
    std::vector<uint8_t> out(512);
    size_t n = sb_crypto_rsa_pkcs1v15_encrypt(publicKey, keyLength, data, length, out.data(),
                                              out.size());
    if (n == 0) {
        return {};
    }
    out.resize(n);
    return out;
}

std::vector<uint8_t> ecdhP256(const uint8_t *peerKey, size_t length) {
    if (length != 64) {
        return {};
    }
    std::vector<uint8_t> out(96);
    if (!sb_crypto_ecdh_p256(peerKey, out.data())) {
        return {};
    }
    return out;
}

}  // namespace GipCrypto

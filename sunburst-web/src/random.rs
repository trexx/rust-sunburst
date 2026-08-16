// SPDX-License-Identifier: GPL-2.0-or-later

//! Unguessable values: the admin token, pairing PINs, and handshake nonces.
//!
//! All three gate something that matters, so all three come from the OS CSPRNG.
//! A hash of the clock would be predictable to anyone who knows roughly when the
//! server started, which for a machine that autostarts at logon is everyone.

/// Failure to read the OS random source.
///
/// Not recoverable and not worth carrying on past: every caller here is
/// producing a secret, and a fallback would produce a guessable one.
#[derive(Debug, thiserror::Error)]
#[error("could not read the OS random source: {0}")]
pub struct RandomError(#[from] getrandom::Error);

pub fn fill(buf: &mut [u8]) -> Result<(), RandomError> {
    getrandom::fill(buf)?;
    Ok(())
}

/// A 128-bit admin token, hex encoded.
pub fn token() -> Result<String, RandomError> {
    let mut bytes = [0u8; 16];
    fill(&mut bytes)?;
    let mut out = String::with_capacity(32);
    for b in bytes {
        out.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(b & 0xF), 16).unwrap_or('0'));
    }
    Ok(out)
}

/// Digits in a pairing PIN.
///
/// Eight rather than four. It is the same effort to read off a TV and type into
/// a browser, and it is four orders of magnitude on the one number that an
/// offline attack gets to grind against — see the pairing note in
/// [`crate::pairing`].
pub const PIN_DIGITS: usize = 8;

/// A uniformly distributed 8-digit PIN, as a zero-padded string.
pub fn pin() -> Result<String, RandomError> {
    // Rejection sampling. Taking `u32 % 100_000_000` would make low PINs very
    // slightly more likely, and biasing the one secret an attacker is already
    // allowed to grind is the wrong place to be approximate.
    const LIMIT: u32 = 100_000_000;
    const CEILING: u32 = u32::MAX - (u32::MAX % LIMIT);

    loop {
        let mut bytes = [0u8; 4];
        fill(&mut bytes)?;
        let value = u32::from_le_bytes(bytes);
        if value < CEILING {
            return Ok(format!("{:0width$}", value % LIMIT, width = PIN_DIGITS));
        }
    }
}

/// A 128-bit nonce for the pairing and session handshakes.
pub fn nonce() -> Result<[u8; 16], RandomError> {
    let mut bytes = [0u8; 16];
    fill(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn tokens_are_hex_and_long_enough() {
        let t = token().expect("token");
        assert_eq!(t.len(), 32, "128 bits of hex");
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn tokens_do_not_repeat() {
        let seen: HashSet<_> = (0..64).map(|_| token().expect("token")).collect();
        assert_eq!(seen.len(), 64, "token generator repeated itself");
    }

    #[test]
    fn pins_are_always_eight_digits() {
        // Zero padding matters: a PIN that renders as "42" on the TV and is
        // stored as "00000042" would never match.
        for _ in 0..500 {
            let p = pin().expect("pin");
            assert_eq!(p.len(), PIN_DIGITS, "got {p}");
            assert!(p.chars().all(|c| c.is_ascii_digit()), "got {p}");
        }
    }

    #[test]
    fn pins_cover_the_range_rather_than_clustering() {
        // A crude check that rejection sampling did not go wrong and leave every
        // PIN in one part of the space.
        let pins: Vec<u32> = (0..200)
            .map(|_| pin().expect("pin").parse().expect("digits"))
            .collect();
        assert!(pins.iter().any(|&p| p < 50_000_000), "no low PINs in 200");
        assert!(pins.iter().any(|&p| p >= 50_000_000), "no high PINs in 200");
    }

    #[test]
    fn pins_do_not_repeat_over_a_short_run() {
        let seen: HashSet<_> = (0..200).map(|_| pin().expect("pin")).collect();
        assert!(
            seen.len() > 190,
            "suspicious repetition: {} unique",
            seen.len()
        );
    }

    #[test]
    fn nonces_do_not_repeat() {
        let seen: HashSet<_> = (0..64).map(|_| nonce().expect("nonce")).collect();
        assert_eq!(seen.len(), 64);
    }
}

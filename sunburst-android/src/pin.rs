// SPDX-License-Identifier: GPL-2.0-or-later

//! PIN generation — pure, host-testable, no platform.
//!
//! Eight digits, uniformly, by rejection sampling: biasing the one secret an
//! attacker may grind offline is the wrong place to be approximate (the same
//! reasoning `fakeclient` and the server's PIN paths use).

use sunburst_core::proto::pairing::PIN_DIGITS;

/// A uniformly-random `PIN_DIGITS`-digit PIN, zero-padded.
pub fn generate_pin() -> Result<String, String> {
    const LIMIT: u32 = 100_000_000; // 10^8, matches PIN_DIGITS = 8
    const CEILING: u32 = u32::MAX - (u32::MAX % LIMIT);
    debug_assert_eq!(PIN_DIGITS, 8);
    loop {
        let mut bytes = [0u8; 4];
        getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
        let value = u32::from_le_bytes(bytes);
        if value < CEILING {
            return Ok(format!("{:0width$}", value % LIMIT, width = PIN_DIGITS));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pin_is_eight_digits() {
        for _ in 0..1000 {
            let pin = generate_pin().expect("rng");
            assert_eq!(pin.len(), 8);
            assert!(pin.chars().all(|c| c.is_ascii_digit()));
        }
    }

    #[test]
    fn pins_vary() {
        // Not a randomness test, just a sanity check that it is not constant.
        let a = generate_pin().unwrap();
        let b = generate_pin().unwrap();
        let c = generate_pin().unwrap();
        assert!(!(a == b && b == c));
    }
}

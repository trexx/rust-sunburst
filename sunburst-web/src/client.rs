// SPDX-License-Identifier: GPL-2.0-or-later

//! Paired clients and their long-lived secrets.

use serde::{Deserialize, Serialize};
use sunburst_core::proto::DecoderQuirks;

/// A device that has completed pairing.
///
/// The secret here is the root of trust for the whole input path: every
/// authenticated packet's key derives from it. Revoking a client must actually
/// remove this record — see [`super::store::Store::save_clients`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairedClient {
    pub id: u32,
    /// Whatever the user called it in the web UI.
    pub name: String,
    /// `Build.MODEL`, for telling two identical-looking boxes apart.
    pub model: String,
    pub abi: String,
    pub quirks: QuirksRecord,
    /// Unix seconds. Shown in the UI so a stale pairing is visible, which is one
    /// of the two free mitigations for PIN-derived pairing.
    pub paired_at: u64,
    pub last_seen: Option<u64>,
    #[serde(with = "hex32")]
    pub secret: [u8; 32],
}

impl PairedClient {
    /// Everything except the secret, for the API.
    ///
    /// The secret must never reach a response body or a log line, so the split
    /// is a type rather than a convention someone has to remember.
    pub fn public(&self) -> PublicClient {
        PublicClient {
            id: self.id,
            name: self.name.clone(),
            model: self.model.clone(),
            abi: self.abi.clone(),
            quirks: self.quirks,
            paired_at: self.paired_at,
            last_seen: self.last_seen,
        }
    }
}

/// A client as the API renders it. No secret, by construction.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublicClient {
    pub id: u32,
    pub name: String,
    pub model: String,
    pub abi: String,
    pub quirks: QuirksRecord,
    pub paired_at: u64,
    pub last_seen: Option<u64>,
}

/// Serde mirror of [`DecoderQuirks`].
///
/// Mirrored rather than derived on the original so that `serde` stays out of
/// `sunburst-core`, which the frame path depends on. The conversions below are
/// the only place the two definitions have to agree, and the test at the bottom
/// of this file is what keeps them agreeing.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct QuirksRecord {
    pub ref_invalidation: bool,
    pub intra_refresh: bool,
    pub slice_output: bool,
    pub needs_annexb_startcodes: bool,
    pub max_bitrate_hint: u32,
}

impl Default for QuirksRecord {
    fn default() -> Self {
        DecoderQuirks::default().into()
    }
}

impl From<DecoderQuirks> for QuirksRecord {
    fn from(q: DecoderQuirks) -> Self {
        QuirksRecord {
            ref_invalidation: q.ref_invalidation,
            intra_refresh: q.intra_refresh,
            slice_output: q.slice_output,
            needs_annexb_startcodes: q.needs_annexb_startcodes,
            max_bitrate_hint: q.max_bitrate_hint,
        }
    }
}

impl From<QuirksRecord> for DecoderQuirks {
    fn from(q: QuirksRecord) -> Self {
        DecoderQuirks {
            ref_invalidation: q.ref_invalidation,
            intra_refresh: q.intra_refresh,
            slice_output: q.slice_output,
            needs_annexb_startcodes: q.needs_annexb_startcodes,
            max_bitrate_hint: q.max_bitrate_hint,
        }
    }
}

/// Hex for the 32-byte secret.
///
/// Hand-rolled rather than pulling a crate for sixteen characters of alphabet.
mod hex32 {
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        let mut out = String::with_capacity(64);
        for b in bytes {
            out.push(nibble(b >> 4));
            out.push(nibble(b & 0xF));
        }
        s.serialize_str(&out)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let text = String::deserialize(d)?;
        if text.len() != 64 {
            return Err(D::Error::custom(format!(
                "expected 64 hex characters, found {}",
                text.len()
            )));
        }
        let mut out = [0u8; 32];
        let raw = text.as_bytes();
        for (i, slot) in out.iter_mut().enumerate() {
            let hi = value(raw[i * 2]).ok_or_else(|| D::Error::custom("not hex"))?;
            let lo = value(raw[i * 2 + 1]).ok_or_else(|| D::Error::custom("not hex"))?;
            *slot = (hi << 4) | lo;
        }
        Ok(out)
    }

    fn nibble(v: u8) -> char {
        char::from_digit(u32::from(v), 16).unwrap_or('0')
    }

    fn value(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> PairedClient {
        PairedClient {
            id: 1,
            name: "Living room".into(),
            model: "SHIELD Android TV".into(),
            abi: "arm64-v8a".into(),
            quirks: QuirksRecord::default(),
            paired_at: 1_700_000_000,
            last_seen: None,
            secret: [0xAB; 32],
        }
    }

    #[test]
    fn round_trips_through_json() {
        let c = client();
        let json = serde_json::to_string(&c).expect("serialise");
        assert_eq!(
            serde_json::from_str::<PairedClient>(&json).expect("load"),
            c
        );
    }

    #[test]
    fn the_secret_is_hex_not_a_byte_array() {
        let json = serde_json::to_string(&client()).expect("serialise");
        assert!(json.contains(&"ab".repeat(32)), "got {json}");
    }

    #[test]
    fn every_byte_value_survives_the_hex_round_trip() {
        let mut c = client();
        for (i, b) in c.secret.iter_mut().enumerate() {
            *b = i as u8;
        }
        let json = serde_json::to_string(&c).expect("serialise");
        assert_eq!(
            serde_json::from_str::<PairedClient>(&json).expect("load"),
            c
        );
    }

    #[test]
    fn a_truncated_secret_is_refused() {
        // Half a secret is not a client with a shorter key; it is a corrupt file,
        // and loading it would authenticate nothing correctly.
        let json = r#"{"id":1,"name":"x","model":"x","abi":"x",
            "quirks":{"ref_invalidation":false,"intra_refresh":false,
            "slice_output":false,"needs_annexb_startcodes":true,"max_bitrate_hint":0},
            "paired_at":0,"last_seen":null,"secret":"abcd"}"#;
        assert!(serde_json::from_str::<PairedClient>(json).is_err());
    }

    #[test]
    fn non_hex_in_the_secret_is_refused() {
        let json = format!(
            r#"{{"id":1,"name":"x","model":"x","abi":"x",
            "quirks":{{"ref_invalidation":false,"intra_refresh":false,
            "slice_output":false,"needs_annexb_startcodes":true,"max_bitrate_hint":0}},
            "paired_at":0,"last_seen":null,"secret":"{}"}}"#,
            "zz".repeat(32)
        );
        assert!(serde_json::from_str::<PairedClient>(&json).is_err());
    }

    #[test]
    fn the_public_view_cannot_carry_the_secret() {
        // Serialising the public view must not produce the secret in any form.
        // Checked against the full hex rather than a fragment of it: "ab" also
        // occurs in the field name "abi", which made the first version of this
        // fail on a response that was perfectly correct.
        let c = client();
        let hex: String = c.secret.iter().map(|b| format!("{b:02x}")).collect();
        let json = serde_json::to_string(&c.public()).expect("serialise");

        assert!(
            !json.contains(&hex),
            "secret leaked into the API shape: {json}"
        );
        assert!(
            !json.contains("secret"),
            "even the field name should be absent"
        );
        assert!(json.contains("Living room"), "the useful fields survived");
    }

    #[test]
    fn quirks_survive_the_round_trip_to_core_and_back() {
        // The mirror exists so serde stays out of sunburst-core. This is what
        // catches the two definitions drifting apart.
        let original = DecoderQuirks {
            ref_invalidation: true,
            intra_refresh: true,
            slice_output: true,
            needs_annexb_startcodes: false,
            max_bitrate_hint: 150_000_000,
        };
        let there: QuirksRecord = original.into();
        let back: DecoderQuirks = there.into();
        assert_eq!(back, original);
    }

    #[test]
    fn quirks_default_matches_cores_conservative_decoder() {
        // An unknown device must not be assumed capable on this side either.
        let mirrored: DecoderQuirks = QuirksRecord::default().into();
        assert_eq!(mirrored, DecoderQuirks::default());
        assert!(!mirrored.ref_invalidation);
    }
}

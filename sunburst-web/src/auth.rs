// SPDX-License-Identifier: GPL-2.0-or-later

//! Bearer-token authorisation for `/api/*`.
//!
//! This API launches processes and manages the server that injects input, so it
//! is at least as sensitive as the UDP port CLAUDE.md insists on authenticating.
//! There is no TLS: the token is a shared-secret gate on a LAN we already treat
//! as trusted for unencrypted video, not confidentiality on the wire.

use subtle::ConstantTimeEq;

use crate::config::WebConfig;

/// How requests are authorised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Auth {
    /// Loopback with no token configured. Nothing off this machine can reach it.
    Open,
    Token(String),
    /// Reachable but unauthenticated — refuse everything.
    ///
    /// [`crate::config::Config::validate`] rejects this combination before it
    /// can be saved, so it should be unreachable. It exists so that if it ever
    /// is reached, the failure is a closed door rather than an open one.
    Denied,
}

impl Auth {
    pub fn from_config(web: &WebConfig) -> Auth {
        match (web.token.is_empty(), web.is_lan_exposed()) {
            (false, _) => Auth::Token(web.token.clone()),
            (true, false) => Auth::Open,
            (true, true) => Auth::Denied,
        }
    }

    /// Whether a request carrying this `Authorization` header may proceed.
    pub fn authorize(&self, header: Option<&str>) -> bool {
        match self {
            Auth::Open => true,
            Auth::Denied => false,
            Auth::Token(expected) => match bearer(header) {
                Some(provided) => verify(expected, provided),
                None => false,
            },
        }
    }
}

/// The token out of `Authorization: Bearer <token>`.
///
/// The scheme name is matched case-insensitively, as RFC 7235 requires.
pub fn bearer(header: Option<&str>) -> Option<&str> {
    let value = header?.trim();
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() { None } else { Some(token) }
}

/// Constant-time token comparison, independent of length.
///
/// Hashing both sides first is what makes the length independent. Comparing the
/// strings directly would have to reject a mismatched length up front, and that
/// early return is itself the leak: it tells an attacker the token's length for
/// free, which is the first thing you would want to know before grinding it.
pub fn verify(expected: &str, provided: &str) -> bool {
    let a = blake3::hash(expected.as_bytes());
    let b = blake3::hash(provided.as_bytes());
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn lan_config(token: &str) -> WebConfig {
        WebConfig {
            bind: IpAddr::V4(Ipv4Addr::new(192, 168, 0, 10)),
            token: token.into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_correct_token_is_accepted() {
        let auth = Auth::from_config(&lan_config("correct-horse"));
        assert!(auth.authorize(Some("Bearer correct-horse")));
    }

    #[test]
    fn a_wrong_token_is_refused() {
        let auth = Auth::from_config(&lan_config("correct-horse"));
        assert!(!auth.authorize(Some("Bearer wrong-horse")));
    }

    #[test]
    fn a_token_of_the_wrong_length_is_refused() {
        // Called out on its own because a naive comparison returns early on a
        // length mismatch, and that early return leaks the length.
        let auth = Auth::from_config(&lan_config("correct-horse"));
        assert!(!auth.authorize(Some("Bearer c")));
        assert!(!auth.authorize(Some("Bearer correct-horse-battery-staple")));
        assert!(!auth.authorize(Some("Bearer correct-hors")));
    }

    #[test]
    fn a_prefix_of_the_token_is_refused() {
        // The case a length-then-content comparison gets right by accident and
        // a byte-at-a-time one leaks through.
        assert!(!verify("correct-horse", "correct-hors"));
        assert!(!verify("correct-horse", "correct-horsee"));
        assert!(verify("correct-horse", "correct-horse"));
    }

    #[test]
    fn a_missing_header_is_refused() {
        let auth = Auth::from_config(&lan_config("tok"));
        assert!(!auth.authorize(None));
        assert!(!auth.authorize(Some("")));
    }

    #[test]
    fn a_malformed_header_is_refused() {
        let auth = Auth::from_config(&lan_config("tok"));
        assert!(!auth.authorize(Some("tok")), "no scheme");
        assert!(!auth.authorize(Some("Basic tok")), "wrong scheme");
        assert!(!auth.authorize(Some("Bearer")), "no token");
        assert!(!auth.authorize(Some("Bearer ")), "empty token");
    }

    #[test]
    fn the_scheme_is_case_insensitive_but_the_token_is_not() {
        let auth = Auth::from_config(&lan_config("MixedCase"));
        assert!(auth.authorize(Some("bearer MixedCase")));
        assert!(auth.authorize(Some("BEARER MixedCase")));
        assert!(!auth.authorize(Some("Bearer mixedcase")));
    }

    #[test]
    fn loopback_without_a_token_is_open() {
        let auth = Auth::from_config(&WebConfig::default());
        assert_eq!(auth, Auth::Open);
        assert!(auth.authorize(None));
    }

    #[test]
    fn loopback_with_a_token_still_requires_it() {
        let web = WebConfig {
            token: "tok".into(),
            ..Default::default()
        };
        assert!(!Auth::from_config(&web).authorize(None));
        assert!(Auth::from_config(&web).authorize(Some("Bearer tok")));
    }

    #[test]
    fn exposed_without_a_token_refuses_everything() {
        // Unreachable through `Config::validate`, which is the point: if the
        // guard is ever bypassed the door is shut, not wedged open.
        let auth = Auth::from_config(&lan_config(""));
        assert_eq!(auth, Auth::Denied);
        assert!(!auth.authorize(None));
        assert!(!auth.authorize(Some("Bearer anything")));
        assert!(!auth.authorize(Some("Bearer ")));
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        // Some clients add it; rejecting a valid token over a stray space would
        // be a confusing failure to debug over a LAN.
        let auth = Auth::from_config(&lan_config("tok"));
        assert!(auth.authorize(Some("  Bearer tok  ")));
    }
}

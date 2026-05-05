//! Webhook Signature Generation and Verification.
//!
//! # Why do we need signing?
//!
//! The `outbox-dispatcher` delivers events to downstream HTTP endpoints. Because these
//! endpoints are often exposed to the internet, receivers need a way to cryptographically
//! verify that an incoming webhook was legitimately sent by the dispatcher and not a
//! malicious actor spoofing requests.
//!
//! This module implements an HMAC-SHA256 signing scheme (similar to Stripe's webhook signing)
//! that provides three critical security guarantees:
//!
//! 1. **Authenticity:** Only a party possessing the shared secret can generate a valid signature.
//! 2. **Integrity:** The signature is computed over the exact byte representation of the request body,
//!    ensuring the payload hasn't been tampered with in transit.
//! 3. **Replay Protection:** The signed payload is prefixed with a UNIX timestamp. Receivers
//!    should parse this timestamp and reject requests that are too old (e.g., older than 5 minutes),
//!    preventing attackers from capturing and re-sending historical webhooks.
//!
//! # Constant-Time Verification
//!
//! To prevent timing attacks, signatures must be compared in constant time. A naive string
//! comparison (`==`) would leak the secret one byte at a time by returning early on the first mismatch.
//! The `verify` function here uses `hmac::Mac::verify_slice` to securely compare the digests.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Computes `X-Outbox-Signature: t=<unix_ts>,v1=<hex(HMAC-SHA256(secret, "<ts>.<body>"))>`.
pub fn sign(secret: &[u8], timestamp_secs: u64, body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(format!("{timestamp_secs}.").as_bytes());
    mac.update(body);
    let digest = mac.finalize().into_bytes();
    format!("t={timestamp_secs},v1={}", hex::encode(digest))
}

/// Verifies a signature header in constant time. Returns `true` if valid.
///
/// Uses `hmac::Mac::verify_slice` to avoid timing side-channels — never `==`
/// on hex strings.
///
/// The caller is responsible for:
/// 1. Extracting the `t=<unix_ts>` field from `header_value` and passing it as
///    `timestamp_secs`. A mismatch between the two means the computed HMAC will
///    not match, so the signature will correctly fail verification.
/// 2. Enforcing a replay window (e.g. reject if `|now() - timestamp_secs| > 300s`).
pub fn verify(secret: &[u8], timestamp_secs: u64, body: &[u8], header_value: &str) -> bool {
    // Parse "t=<ts>,v1=<hex>"
    let hex_digest = match parse_v1_digest(header_value) {
        Some(d) => d,
        None => return false,
    };
    let decoded = match hex::decode(hex_digest) {
        Ok(b) => b,
        Err(_) => return false,
    };

    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(format!("{timestamp_secs}.").as_bytes());
    mac.update(body);
    mac.verify_slice(&decoded).is_ok()
}

/// High-level verifier that handles timestamp extraction and replay-window enforcement.
///
/// Receivers should call this instead of [`verify`] directly. It:
/// 1. Parses `t=<unix_ts>` from the header.
/// 2. Rejects the request if the timestamp is older than `max_age`.
/// 3. Delegates HMAC verification to [`verify`] in constant time.
///
/// Returns `true` only when the signature is valid **and** within the replay window.
pub fn verify_header(secret: &[u8], body: &[u8], header_value: &str, max_age: Duration) -> bool {
    let Some(ts) = parse_t_field(header_value) else {
        return false;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if now.saturating_sub(ts) > max_age.as_secs() {
        return false;
    }
    verify(secret, ts, body, header_value)
}

fn parse_t_field(header_value: &str) -> Option<u64> {
    for part in header_value.split(',') {
        if let Some(ts) = part.trim().strip_prefix("t=") {
            return ts.parse().ok();
        }
    }
    None
}

fn parse_v1_digest(header_value: &str) -> Option<&str> {
    for part in header_value.split(',') {
        if let Some(hex) = part.trim().strip_prefix("v1=") {
            return Some(hex);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"super-secret-key-32-bytes-minimum!!";

    #[test]
    fn sign_and_verify_roundtrip() {
        let body = b"{\"hello\":\"world\"}";
        let ts = 1_714_229_400_u64;
        let header = sign(SECRET, ts, body);
        assert!(verify(SECRET, ts, body, &header));
    }

    #[test]
    fn verify_rejects_wrong_secret() {
        let body = b"{\"hello\":\"world\"}";
        let ts = 1_714_229_400_u64;
        let header = sign(SECRET, ts, body);
        assert!(!verify(b"wrong-secret", ts, body, &header));
    }

    #[test]
    fn verify_header_roundtrip() {
        let body = b"{\"hello\":\"world\"}";
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let header = sign(SECRET, ts, body);
        assert!(verify_header(
            SECRET,
            body,
            &header,
            Duration::from_secs(300)
        ));
    }

    #[test]
    fn verify_header_rejects_old_signature() {
        let body = b"{\"hello\":\"world\"}";
        // timestamp one hour in the past
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(3601);
        let header = sign(SECRET, ts, body);
        assert!(!verify_header(
            SECRET,
            body,
            &header,
            Duration::from_secs(300)
        ));
    }

    #[test]
    fn verify_header_rejects_wrong_secret() {
        let body = b"{\"hello\":\"world\"}";
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let header = sign(SECRET, ts, body);
        assert!(!verify_header(
            b"wrong-secret",
            body,
            &header,
            Duration::from_secs(300)
        ));
    }

    #[test]
    fn verify_header_rejects_missing_t_field() {
        // A header with v1= but no t= is rejected before HMAC verification.
        assert!(!verify_header(
            SECRET,
            b"body",
            "v1=aabbcc",
            Duration::from_secs(300)
        ));
    }

    #[test]
    fn verify_rejects_single_byte_flip() {
        let body = b"{\"hello\":\"world\"}";
        let ts = 1_714_229_400_u64;
        let header = sign(SECRET, ts, body);

        // Flip one byte in the hex digest.
        let flipped = header.replacen('a', "b", 1);
        let flipped = if flipped == header {
            header.replacen('0', "1", 1)
        } else {
            flipped
        };

        assert!(!verify(SECRET, ts, body, &flipped));
    }
}

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

//! Request signing.
//!
//! Scheme (version `v1`), the same shape Stripe and GitHub use so receivers can
//! reuse what they already have:
//!
//! ```text
//! X-Mallard-Timestamp: <unix seconds, decimal>
//! X-Mallard-Signature: v1=<lowercase hex HMAC-SHA256(secret, "<timestamp>.<raw body>")>
//! ```
//!
//! The timestamp is part of the signed text and is refreshed on every retry, so
//! a receiver can reject a captured request that is replayed later. The
//! delivery id (`X-Mallard-Delivery`) stays the same across retries, for
//! de-duplication; it is not signed separately because it is inside the body.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// The `X-Mallard-Signature` header value for `body` sent at `timestamp`.
pub fn signature(secret: &[u8], timestamp: i64, body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    format!("v1={}", hex::encode(mac.finalize().into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_openssl() {
        // printf '1700000000.{"a":1}' | openssl dgst -sha256 -hmac 'k'
        assert_eq!(
            signature(b"k", 1_700_000_000, br#"{"a":1}"#),
            "v1=1b6ad1bc9bf48c52ce01939866d5877c6cc4e87ddad27e16673cb8c4964394a3"
        );
    }

    #[test]
    fn covers_timestamp_and_body() {
        let base = signature(b"secret", 100, b"body");
        assert_ne!(base, signature(b"secret", 101, b"body"));
        assert_ne!(base, signature(b"secret", 100, b"bodY"));
        assert_ne!(base, signature(b"other", 100, b"body"));
        assert_eq!(base, signature(b"secret", 100, b"body"));
    }
}

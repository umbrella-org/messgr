//! Generic webhook signature verification (T-047 decision 2). Provider
//! selection is still open (`14-decisions-and-open-questions.md` #4), so
//! this proves the verification *shape* against a mock provider rather than
//! a named vendor's actual scheme -- the same treatment T-012 gave the
//! outbound side (`src/sender/http.rs`'s `HttpSender`).

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// Verifies a webhook request's signature against the tenant's shared
/// secret. `secret` and `body` are the raw bytes the provider signed (never
/// the parsed payload); `signature_header` is the raw header value exactly
/// as the provider sent it.
pub trait WebhookVerifier: Send + Sync {
    fn verify(&self, secret: &[u8], body: &[u8], signature_header: &str) -> bool;
}

/// Shared-secret HMAC-SHA256 over the raw request body, hex-encoded (an
/// optional leading `sha256=` is stripped first -- the common GitHub-style
/// convention, not any one real provider's exact scheme). Reuses the same
/// `hmac`/`sha2` crates `destination_hmac.rs` already depends on.
pub struct HmacSha256Verifier;

impl WebhookVerifier for HmacSha256Verifier {
    fn verify(&self, secret: &[u8], body: &[u8], signature_header: &str) -> bool {
        let hex_digest = signature_header
            .strip_prefix("sha256=")
            .unwrap_or(signature_header);
        let Ok(signature_bytes) = hex_decode(hex_digest) else {
            return false;
        };

        let Ok(mut mac) = <Hmac<Sha256>>::new_from_slice(secret) else {
            return false;
        };
        mac.update(body);
        // `verify_slice` compares in constant time -- never a manual `==`
        // over the decoded bytes.
        mac.verify_slice(&signature_bytes).is_ok()
    }
}

fn hex_decode(s: &str) -> Result<Vec<u8>, ()> {
    if !s.len().is_multiple_of(2) {
        return Err(());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| ()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(secret: &[u8], body: &[u8]) -> String {
        let mut mac = <Hmac<Sha256>>::new_from_slice(secret).unwrap();
        mac.update(body);
        let bytes = mac.finalize().into_bytes();
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn accepts_a_correctly_signed_body() {
        let secret = b"tenant-secret";
        let body = b"{\"provider_ref\":\"abc\"}";
        let signature = sign(secret, body);
        assert!(HmacSha256Verifier.verify(secret, body, &signature));
    }

    #[test]
    fn accepts_the_sha256_prefixed_form() {
        let secret = b"tenant-secret";
        let body = b"payload";
        let signature = format!("sha256={}", sign(secret, body));
        assert!(HmacSha256Verifier.verify(secret, body, &signature));
    }

    #[test]
    fn rejects_a_wrong_secret() {
        let body = b"payload";
        let signature = sign(b"tenant-secret", body);
        assert!(!HmacSha256Verifier.verify(b"wrong-secret", body, &signature));
    }

    #[test]
    fn rejects_a_tampered_body() {
        let secret = b"tenant-secret";
        let signature = sign(secret, b"original");
        assert!(!HmacSha256Verifier.verify(secret, b"tampered", &signature));
    }

    #[test]
    fn rejects_a_malformed_header() {
        assert!(!HmacSha256Verifier.verify(b"secret", b"payload", "not-hex!!"));
    }
}

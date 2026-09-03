//! Channel-agnostic outbound send abstraction (DESIGN.md §14 step 11: email
//! and WhatsApp implement this same trait later). `HttpSender` (T-012) is
//! the first concrete implementation, shaped for SMS-style short text
//! bodies but not type-restricted to them. No real vendor is wired up yet —
//! DESIGN.md leaves provider selection open (Still Open #5).

use async_trait::async_trait;

pub mod http;

/// What a successful send reports back, mapped directly onto
/// `comms_event.provider_ref`/`provider_status` (DESIGN.md §4.4) by
/// whichever future ticket writes that row (T-013).
#[derive(Debug, Clone, PartialEq)]
pub struct SendOutcome {
    pub provider_ref: String,
    pub provider_status: String,
}

#[derive(Debug)]
pub enum SenderError {
    /// Transport-level failure: connection refused, timeout, response body
    /// that doesn't decode as the expected shape.
    Http(reqwest::Error),
    /// The provider responded, but not with success.
    Provider { status: u16, body: String },
}

impl std::fmt::Display for SenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(err) => write!(f, "sender transport error: {err}"),
            Self::Provider { status, body } => {
                write!(f, "sender provider error: status {status}, body {body:?}")
            }
        }
    }
}

impl std::error::Error for SenderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Http(err) => Some(err),
            Self::Provider { .. } => None,
        }
    }
}

impl From<reqwest::Error> for SenderError {
    fn from(err: reqwest::Error) -> Self {
        Self::Http(err)
    }
}

impl SenderError {
    /// DESIGN.md §2.4 step 6 (T-021 decision 1): a 4xx-equivalent provider
    /// rejection is a permanent rejection (bad destination, bad payload,
    /// auth failure with this credential) and terminal; a transport-level
    /// failure or a 5xx-equivalent provider status is treated as transient
    /// and retried with backoff.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Http(_) => true,
            Self::Provider { status, .. } => *status >= 500,
        }
    }
}

/// A destination to send `body` to, and the provider's outcome or error.
#[async_trait]
pub trait Sender: Send + Sync {
    async fn send(
        &self,
        destination: &str,
        body: &str,
    ) -> Result<SendOutcome, SenderError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(status: u16) -> SenderError {
        SenderError::Provider {
            status,
            body: String::new(),
        }
    }

    #[test]
    fn provider_status_retryability_boundary() {
        assert!(!provider(400).is_retryable(), "4xx is terminal");
        assert!(!provider(499).is_retryable(), "still 4xx, terminal");
        assert!(provider(500).is_retryable(), "5xx is retryable");
        assert!(provider(599).is_retryable(), "still 5xx, retryable");
    }

    #[tokio::test]
    async fn http_transport_error_is_always_retryable() {
        let client = reqwest::Client::new();
        let err = client
            .get("http://127.0.0.1:1")
            .send()
            .await
            .expect_err("connecting to a closed port must fail");
        assert!(SenderError::from(err).is_retryable());
    }
}

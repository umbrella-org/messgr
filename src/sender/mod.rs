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

/// A destination to send `body` to, and the provider's outcome or error.
#[async_trait]
pub trait Sender: Send + Sync {
    async fn send(
        &self,
        destination: &str,
        body: &str,
    ) -> Result<SendOutcome, SenderError>;
}

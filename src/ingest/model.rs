use std::collections::HashMap;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::customer_dek::lifecycle::CustomerDekError;
use crate::encryption::EncryptionError;
use crate::keystore::KeyStoreError;
use crate::producer::resolve::ResolutionError;
use crate::template::render::RenderError;
use crate::tenant::registry::RegistryError;

pub mod class {
    pub const TRANSACTIONAL: &str = "transactional";
    pub const MARKETING: &str = "marketing";
    /// Never a legal value for `POST /comms` — auth/OTP never goes through
    /// the queue (AGENTS.md hard invariant 1); its real path is
    /// `sms-sender`/`otp-api` (T-047, unbuilt). Kept here, not omitted, so
    /// `handler::create_comms`'s rejection can name it in the error message
    /// rather than leaving an unmatched value to a generic "invalid class".
    pub const AUTH: &str = "auth";
}

pub mod channel {
    pub const SMS: &str = "sms";
    pub const EMAIL: &str = "email";
    pub const WHATSAPP: &str = "whatsapp";
}

#[derive(Debug, Deserialize)]
pub struct CreateCommsRequest {
    pub customer_id: Uuid,
    pub destination: String,
    pub channel: String,
    pub class: String,
    pub template_id: String,
    pub template_version: i32,
    pub locale: Option<String>,
    pub campaign_id: Option<String>,
    #[serde(default)]
    pub variables: HashMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct CreateCommsResponse {
    pub comms_request_id: Uuid,
}

#[derive(Debug)]
pub enum IngestError {
    /// The mTLS layer's `PeerCertSubject` extension is missing — should be
    /// unreachable once the handshake completed (see `mtls::PeerCertSubject`'s
    /// own doc comment); surfaced as a `500`, not a `403`, because it means
    /// the server is mis-wired, not that the caller did anything wrong.
    MissingPeerCertificate,
    UnknownProducer,
    ProducerDisabled,
    TenantNotConfigured,
    MissingIdempotencyKey,
    InvalidClass(String),
    InvalidChannel(String),
    CampaignIdOnTransactional,
    TemplateNotFound,
    Render(RenderError),
    Database(sqlx::Error),
    Encryption(EncryptionError),
    Vault(KeyStoreError),
}

impl From<ResolutionError> for IngestError {
    fn from(err: ResolutionError) -> Self {
        match err {
            ResolutionError::UnknownCert => Self::UnknownProducer,
            ResolutionError::Disabled { .. } => Self::ProducerDisabled,
            ResolutionError::Database(err) => Self::Database(err),
        }
    }
}

impl From<RegistryError> for IngestError {
    fn from(err: RegistryError) -> Self {
        match err {
            RegistryError::UnknownTenant(_) => Self::UnknownProducer,
            RegistryError::NotConfigured(_) => Self::TenantNotConfigured,
            RegistryError::Database(err) => Self::Database(err),
            RegistryError::Vault(err) => Self::Vault(err),
        }
    }
}

impl From<sqlx::Error> for IngestError {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err)
    }
}

impl From<EncryptionError> for IngestError {
    fn from(err: EncryptionError) -> Self {
        Self::Encryption(err)
    }
}

impl From<KeyStoreError> for IngestError {
    fn from(err: KeyStoreError) -> Self {
        Self::Vault(err)
    }
}

impl From<RenderError> for IngestError {
    fn from(err: RenderError) -> Self {
        Self::Render(err)
    }
}

impl From<CustomerDekError> for IngestError {
    fn from(err: CustomerDekError) -> Self {
        match err {
            CustomerDekError::Database(err) => Self::Database(err),
            CustomerDekError::Vault(err) => Self::Vault(err),
        }
    }
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingPeerCertificate => {
                write!(f, "mTLS layer did not attach a peer certificate")
            }
            Self::UnknownProducer => write!(f, "unregistered producer certificate"),
            Self::ProducerDisabled => write!(f, "producer is disabled"),
            Self::TenantNotConfigured => write!(f, "tenant has no tenant_config"),
            Self::MissingIdempotencyKey => {
                write!(f, "Idempotency-Key header is required")
            }
            Self::InvalidClass(class) => write!(f, "invalid class {class:?}"),
            Self::InvalidChannel(channel) => write!(f, "invalid channel {channel:?}"),
            Self::CampaignIdOnTransactional => {
                write!(f, "campaign_id must be null for class=transactional")
            }
            Self::TemplateNotFound => write!(f, "template not found"),
            Self::Render(err) => write!(f, "template render failed: {err}"),
            Self::Database(err) => write!(f, "database error: {err}"),
            Self::Encryption(err) => write!(f, "encryption error: {err}"),
            Self::Vault(err) => write!(f, "vault error: {err}"),
        }
    }
}

impl std::error::Error for IngestError {}

impl IntoResponse for IngestError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::UnknownProducer | Self::ProducerDisabled => StatusCode::FORBIDDEN,
            Self::MissingIdempotencyKey => StatusCode::BAD_REQUEST,
            Self::InvalidClass(_)
            | Self::InvalidChannel(_)
            | Self::CampaignIdOnTransactional => StatusCode::UNPROCESSABLE_ENTITY,
            Self::TemplateNotFound => StatusCode::NOT_FOUND,
            Self::TenantNotConfigured => StatusCode::FAILED_DEPENDENCY,
            Self::MissingPeerCertificate
            | Self::Render(_)
            | Self::Database(_)
            | Self::Encryption(_)
            | Self::Vault(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };

        let body = serde_json::json!({ "error": self.to_string() });
        (status, Json(body)).into_response()
    }
}

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::customer_dek::lifecycle::CustomerDekError;
use crate::encryption::EncryptionError;
use crate::keystore::KeyStoreError;
use crate::producer::resolve::ResolutionError;
use crate::tenant::registry::RegistryError;

use super::provider::ProviderSendError;

/// Fixed for every request this binary handles (decision 4: hardcoded, not
/// imported from `ingest::model::class` -- that module's `AUTH` constant
/// exists only so `messgr-ingest` can name the value it rejects).
pub const CHANNEL: &str = "sms";
pub const CLASS: &str = "auth";
/// The sentinel template (decision 5) approved once per tenant via
/// `messgr-control template approve` -- never rendered, exists only to
/// satisfy `comms_request`'s `NOT NULL` columns.
pub const TEMPLATE_ID: &str = "otp";
pub const TEMPLATE_VERSION: i32 = 1;

#[derive(Debug, Deserialize)]
pub struct SendOtpRequest {
    pub customer_id: Uuid,
    pub destination: String,
    pub body: String,
}

#[derive(Debug, Serialize)]
pub struct SendOtpResponse {
    pub comms_request_id: Uuid,
}

/// One `comms_request`/`comms_event` pair, written either directly
/// (`repo::write_audit_record`) or buffered to local disk on a database
/// failure (`buffer::append`) -- the same shape either way, so a retried
/// drain writes byte-identical values (decision 9's id-idempotency).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditRecord {
    pub tenant_id: Uuid,
    pub comms_request_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub customer_id: Uuid,
    pub producer_id: Uuid,
    pub destination_hmac: Vec<u8>,
    pub destination_ciphertext: Vec<u8>,
    /// "sent" | "failed" | "discarded" -- also used verbatim as
    /// `comms_event.event_type` (decision 10).
    pub final_status: String,
    pub provider_ref: String,
    pub provider_status: Option<String>,
}

/// A send whose audit-record crypto (DEK resolution, HMAC, encryption)
/// could not complete -- the OTP itself was still sent (`handler::send_otp`
/// calls the provider before any of this, F1 rework), so this is buffered
/// (`pending.rs`) and retried until the crypto step succeeds, at which
/// point it becomes an ordinary `AuditRecord` and follows the same
/// write-or-buffer path (`buffer.rs`) as every other send.
///
/// `destination_ciphertext` is encrypted under `pending::derive_key`, not
/// the customer DEK that's unavailable in exactly the outage this buffer
/// exists for (F5 rework) -- it is never written to disk in plaintext.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingAuditRecord {
    pub tenant_id: Uuid,
    pub comms_request_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub customer_id: Uuid,
    pub producer_id: Uuid,
    pub destination_ciphertext: Vec<u8>,
    pub final_status: String,
    pub provider_ref: String,
    pub provider_status: Option<String>,
}

#[derive(Debug)]
pub enum SmsSenderError {
    /// See `ingest::model::IngestError::MissingPeerCertificate` -- identical
    /// reasoning, same "should be unreachable" 500.
    MissingPeerCertificate,
    UnknownProducer,
    ProducerDisabled,
    TenantNotActive,
    TenantNotConfigured,
    /// `tenant.auth_enabled` is `false` (decision 8) -- a discarded record
    /// was already written before this is returned.
    AuthDisabled,
    /// Every configured provider was tried and failed, or none are
    /// configured -- a `failed` record was already written before this is
    /// returned.
    SendFailed,
    Database(sqlx::Error),
    Encryption(EncryptionError),
    Vault(KeyStoreError),
    Dek(CustomerDekError),
}

impl From<ResolutionError> for SmsSenderError {
    fn from(err: ResolutionError) -> Self {
        match err {
            ResolutionError::UnknownCert => Self::UnknownProducer,
            ResolutionError::Disabled { .. } => Self::ProducerDisabled,
            ResolutionError::TenantNotActive { .. } => Self::TenantNotActive,
            ResolutionError::Database(err) => Self::Database(err),
        }
    }
}

impl From<RegistryError> for SmsSenderError {
    fn from(err: RegistryError) -> Self {
        match err {
            RegistryError::UnknownTenant(_) => Self::UnknownProducer,
            RegistryError::NotConfigured(_) => Self::TenantNotConfigured,
            RegistryError::Database(err) => Self::Database(err),
            RegistryError::Vault(err) => Self::Vault(err),
        }
    }
}

impl From<sqlx::Error> for SmsSenderError {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err)
    }
}

impl From<EncryptionError> for SmsSenderError {
    fn from(err: EncryptionError) -> Self {
        Self::Encryption(err)
    }
}

impl From<KeyStoreError> for SmsSenderError {
    fn from(err: KeyStoreError) -> Self {
        Self::Vault(err)
    }
}

impl From<CustomerDekError> for SmsSenderError {
    fn from(err: CustomerDekError) -> Self {
        match err {
            CustomerDekError::Database(err) => Self::Database(err),
            CustomerDekError::Vault(err) => Self::Vault(err),
        }
    }
}

impl From<ProviderSendError> for SmsSenderError {
    fn from(err: ProviderSendError) -> Self {
        match err {
            ProviderSendError::Exhausted(_) => Self::SendFailed,
        }
    }
}

impl std::fmt::Display for SmsSenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingPeerCertificate => {
                write!(f, "mTLS layer did not attach a peer certificate")
            }
            Self::UnknownProducer => write!(f, "unregistered producer certificate"),
            Self::ProducerDisabled => write!(f, "producer is disabled"),
            Self::TenantNotActive => write!(f, "tenant is not active"),
            Self::TenantNotConfigured => write!(f, "tenant has no tenant_config"),
            Self::AuthDisabled => write!(f, "auth is disabled for this tenant"),
            Self::SendFailed => write!(f, "every configured provider failed"),
            Self::Database(err) => write!(f, "database error: {err}"),
            Self::Encryption(err) => write!(f, "encryption error: {err}"),
            Self::Vault(err) => write!(f, "vault error: {err}"),
            Self::Dek(err) => write!(f, "dek error: {err}"),
        }
    }
}

impl std::error::Error for SmsSenderError {}

impl IntoResponse for SmsSenderError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::UnknownProducer | Self::ProducerDisabled | Self::TenantNotActive => {
                StatusCode::FORBIDDEN
            }
            Self::AuthDisabled => StatusCode::SERVICE_UNAVAILABLE,
            Self::SendFailed => StatusCode::BAD_GATEWAY,
            Self::TenantNotConfigured => StatusCode::FAILED_DEPENDENCY,
            Self::MissingPeerCertificate
            | Self::Database(_)
            | Self::Encryption(_)
            | Self::Vault(_)
            | Self::Dek(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };

        let body = serde_json::json!({ "error": self.to_string() });
        (status, Json(body)).into_response()
    }
}

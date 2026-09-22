use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::path::Path;
use uuid::Uuid;

use crate::customer_dek::lifecycle::get_or_create_dek;
use crate::destination_hmac;
use crate::encryption;
use crate::tenant::registry::TenantContext;

use super::AppState;
use super::identity::ProducerContext;
use super::model::{
    AuditRecord, OtpError, PendingAuditRecord, SendOtpRequest, SendOtpResponse,
};
use super::{buffer, pending, provider, repo};

pub async fn send_otp(
    State(app): State<AppState>,
    producer: ProducerContext,
    Json(body): Json<SendOtpRequest>,
) -> Result<(StatusCode, Json<SendOtpResponse>), OtpError> {
    let tenant = producer.tenant;
    let comms_request_id = Uuid::new_v4();
    let created_at = Utc::now();

    // `tenant.auth_enabled` fail-open check, ahead of the provider call --
    // a disabled tenant never reaches the provider at all. Destination is
    // never hashed or encrypted for a discarded row -- empty `bytea` values
    // satisfy the NOT NULL columns without implying any lookup identity for
    // them.
    if !app.auth_flag.is_enabled(tenant.tenant.id).await {
        let record = AuditRecord {
            tenant_id: tenant.tenant.id,
            comms_request_id,
            created_at,
            customer_id: body.customer_id,
            producer_id: producer.producer_id,
            destination_hmac: Vec::new(),
            destination_ciphertext: Vec::new(),
            final_status: "discarded".to_string(),
            provider_ref: String::new(),
            provider_status: None,
        };
        persist_or_buffer(&tenant.pool, &app.buffer_path, &record).await;
        return Err(OtpError::AuthDisabled);
    }

    // The provider call happens before any DEK/Vault/Postgres work for the
    // audit record: the entire point of this path is that a Vault or
    // Postgres outage must never block the OTP itself reaching the customer
    // (AGENTS.md hard invariant 1). Credentials are already resolved --
    // `get_or_fetch` never touches Vault on this request unless this is the
    // first request this process has ever seen for this tenant.
    let resolved = app
        .provider_cache
        .get_or_fetch(&tenant.pool, app.keystore.as_ref(), tenant.tenant.id)
        .await;
    let send_result =
        provider::send(&resolved, &app.otp_base_url, &body.destination, &body.body)
            .await;

    let (final_status, provider_ref, provider_status) = match &send_result {
        Ok(outcome) => (
            "sent".to_string(),
            outcome.provider_ref.clone(),
            Some(outcome.provider_status.clone()),
        ),
        Err(err) => ("failed".to_string(), String::new(), err.provider_status()),
    };

    persist_send_outcome(
        &app,
        &tenant,
        comms_request_id,
        created_at,
        body.customer_id,
        producer.producer_id,
        &body.destination,
        final_status,
        provider_ref,
        provider_status,
    )
    .await;

    match send_result {
        Ok(_) => Ok((
            StatusCode::CREATED,
            Json(SendOtpResponse { comms_request_id }),
        )),
        Err(err) => Err(OtpError::from(err)),
    }
}

/// Resolves the DEK, computes `destination_hmac`/`destination_ciphertext`,
/// and writes (or buffers) the completed `AuditRecord`. That crypto step
/// runs *after* the send and, on its own failure, buffers a
/// `PendingAuditRecord` instead of failing the request: the send already
/// happened, so this must degrade the same way `persist_or_buffer`'s own
/// DB-write failure already does.
#[allow(clippy::too_many_arguments)]
async fn persist_send_outcome(
    app: &AppState,
    tenant: &TenantContext,
    comms_request_id: Uuid,
    created_at: DateTime<Utc>,
    customer_id: Uuid,
    producer_id: Uuid,
    destination: &str,
    final_status: String,
    provider_ref: String,
    provider_status: Option<String>,
) {
    match compute_destination_crypto(
        app,
        tenant,
        comms_request_id,
        customer_id,
        destination,
    )
    .await
    {
        Some((destination_hmac, destination_ciphertext)) => {
            let record = AuditRecord {
                tenant_id: tenant.tenant.id,
                comms_request_id,
                created_at,
                customer_id,
                producer_id,
                destination_hmac,
                destination_ciphertext,
                final_status,
                provider_ref,
                provider_status,
            };
            persist_or_buffer(&tenant.pool, &app.buffer_path, &record).await;
        }
        None => {
            // Encrypted under a key derived from the tenant's already-cached
            // pepper -- never the customer DEK, whose unavailability is
            // exactly why this branch was taken.
            let key = pending::derive_key(&tenant.pepper);
            match encryption::encrypt(
                &key,
                comms_request_id.as_bytes(),
                destination.as_bytes(),
            ) {
                Ok(destination_ciphertext) => {
                    let pending_record = PendingAuditRecord {
                        tenant_id: tenant.tenant.id,
                        comms_request_id,
                        created_at,
                        customer_id,
                        producer_id,
                        destination_ciphertext,
                        final_status,
                        provider_ref,
                        provider_status,
                    };
                    if let Err(io_err) =
                        pending::append(&app.pending_path, &pending_record)
                    {
                        tracing::error!(
                            %io_err,
                            %comms_request_id,
                            "otp: buffering the pending-crypto audit record also failed -- record is lost"
                        );
                    }
                }
                Err(err) => {
                    tracing::error!(
                        %err,
                        %comms_request_id,
                        "otp: encrypting the pending-crypto buffer record failed -- record is lost"
                    );
                }
            }
        }
    }
}

/// `None` on a DEK/Vault/Postgres failure -- logged here, handled by the
/// caller (buffers a `PendingAuditRecord` instead).
async fn compute_destination_crypto(
    app: &AppState,
    tenant: &TenantContext,
    comms_request_id: Uuid,
    customer_id: Uuid,
    destination: &str,
) -> Option<(Vec<u8>, Vec<u8>)> {
    let dek = match get_or_create_dek(
        &tenant.pool,
        app.keystore.as_ref(),
        &tenant.dek_cache,
        &tenant.tenant.vault_mount,
        customer_id,
    )
    .await
    {
        Ok(dek) => dek,
        Err(err) => {
            tracing::error!(
                %err,
                %comms_request_id,
                "otp: DEK resolution failed, buffering the audit record for later encryption"
            );
            return None;
        }
    };

    let aad = comms_request_id.as_bytes();
    let destination_hmac = destination_hmac::compute(&tenant.pepper, destination);
    match encryption::encrypt(&dek, aad, destination.as_bytes()) {
        Ok(destination_ciphertext) => Some((destination_hmac, destination_ciphertext)),
        Err(err) => {
            tracing::error!(
                %err,
                %comms_request_id,
                "otp: encrypting the destination failed, buffering the audit record for later encryption"
            );
            None
        }
    }
}

/// Attempts the direct write; on failure, buffers to local disk instead.
/// Never returns an error to the caller -- the provider outcome is what the
/// caller sees, regardless of how this went.
async fn persist_or_buffer(pool: &PgPool, buffer_path: &Path, record: &AuditRecord) {
    if let Err(err) = repo::write_audit_record(pool, record).await {
        tracing::error!(
            %err,
            comms_request_id = %record.comms_request_id,
            "otp: audit write failed, buffering to local disk"
        );
        if let Err(io_err) = buffer::append(buffer_path, record) {
            tracing::error!(
                %io_err,
                comms_request_id = %record.comms_request_id,
                "otp: buffering the audit record to disk also failed -- record is lost"
            );
        }
    }
}

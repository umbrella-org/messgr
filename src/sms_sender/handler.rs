use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use chrono::Utc;
use sqlx::PgPool;
use std::path::Path;
use uuid::Uuid;

use crate::customer_dek::lifecycle::get_or_create_dek;
use crate::destination_hmac;
use crate::encryption;

use super::AppState;
use super::identity::ProducerContext;
use super::model::{AuditRecord, SendOtpRequest, SendOtpResponse, SmsSenderError};
use super::{buffer, provider, repo};

pub async fn send_otp(
    State(app): State<AppState>,
    producer: ProducerContext,
    Json(body): Json<SendOtpRequest>,
) -> Result<(StatusCode, Json<SendOtpResponse>), SmsSenderError> {
    let tenant = producer.tenant;
    let comms_request_id = Uuid::new_v4();
    let created_at = Utc::now();

    // `tenant.auth_enabled` fail-open check (decision 8) -- ahead of DEK/HMAC
    // work, the same cheapest-first ordering `messgr-ingest`'s kill-switch
    // check uses: a disabled tenant never spends Vault/DEK work on a send
    // that will never reach the provider. Destination is never hashed or
    // encrypted for a discarded row -- empty `bytea` values satisfy the
    // NOT NULL columns without implying any lookup identity for them.
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
        return Err(SmsSenderError::AuthDisabled);
    }

    let dek = get_or_create_dek(
        &tenant.pool,
        app.keystore.as_ref(),
        &tenant.dek_cache,
        &tenant.tenant.vault_mount,
        body.customer_id,
    )
    .await?;

    let aad = comms_request_id.as_bytes();
    let destination_hmac = destination_hmac::compute(&tenant.pepper, &body.destination);
    let destination_ciphertext =
        encryption::encrypt(&dek, aad, body.destination.as_bytes())?;

    // `payload_ciphertext` is always NULL (decision 5) -- `body.body` (the
    // OTP code) is used transiently for the provider call below and never
    // persisted anywhere, encrypted or not (it is a live credential, §7.4).
    let send_result = provider::send(
        &app.provider_config_cache,
        &tenant.pool,
        tenant.tenant.id,
        app.keystore.as_ref(),
        &app.sms_base_url,
        &body.destination,
        &body.body,
    )
    .await;

    let record = match &send_result {
        Ok(outcome) => AuditRecord {
            tenant_id: tenant.tenant.id,
            comms_request_id,
            created_at,
            customer_id: body.customer_id,
            producer_id: producer.producer_id,
            destination_hmac,
            destination_ciphertext,
            final_status: "sent".to_string(),
            provider_ref: outcome.provider_ref.clone(),
            provider_status: Some(outcome.provider_status.clone()),
        },
        Err(err) => AuditRecord {
            tenant_id: tenant.tenant.id,
            comms_request_id,
            created_at,
            customer_id: body.customer_id,
            producer_id: producer.producer_id,
            destination_hmac,
            destination_ciphertext,
            final_status: "failed".to_string(),
            provider_ref: String::new(),
            provider_status: err.provider_status(),
        },
    };

    persist_or_buffer(&tenant.pool, &app.buffer_path, &record).await;

    match send_result {
        Ok(_) => Ok((
            StatusCode::CREATED,
            Json(SendOtpResponse { comms_request_id }),
        )),
        Err(err) => Err(SmsSenderError::from(err)),
    }
}

/// Attempts the direct write; on failure, buffers to local disk instead
/// (decision 9). Never returns an error to the caller -- the provider
/// outcome is what the caller sees, regardless of how this went (task 5).
async fn persist_or_buffer(pool: &PgPool, buffer_path: &Path, record: &AuditRecord) {
    if let Err(err) = repo::write_audit_record(pool, record).await {
        tracing::error!(
            %err,
            comms_request_id = %record.comms_request_id,
            "sms-sender: audit write failed, buffering to local disk"
        );
        if let Err(io_err) = buffer::append(buffer_path, record) {
            tracing::error!(
                %io_err,
                comms_request_id = %record.comms_request_id,
                "sms-sender: buffering the audit record to disk also failed -- record is lost"
            );
        }
    }
}

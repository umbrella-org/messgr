use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use uuid::Uuid;

use crate::customer_dek::lifecycle::get_or_create_dek;
use crate::destination_hmac;
use crate::encryption;
use crate::template::render::render;
use crate::template::repo as template_repo;

use super::AppState;
use super::identity::ProducerContext;
use super::model::{
    CreateCommsRequest, CreateCommsResponse, IngestError, channel, class,
};
use super::repo::{InsertOutcome, insert_transactional};

pub async fn create_comms(
    State(app): State<AppState>,
    producer: ProducerContext,
    headers: HeaderMap,
    Json(body): Json<CreateCommsRequest>,
) -> Result<(StatusCode, Json<CreateCommsResponse>), IngestError> {
    let idempotency_key = headers
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .ok_or(IngestError::MissingIdempotencyKey)?
        .to_string();

    validate_channel(&body.channel)?;
    validate_class(&body.class, body.campaign_id.as_deref())?;

    let tenant = producer.tenant;

    // Fast pre-check: skip template/DEK/encryption work for a genuine retry.
    if let Some(existing) =
        super::repo::find_idempotent_reply(&tenant.pool, &idempotency_key).await?
    {
        return Ok((
            StatusCode::OK,
            Json(CreateCommsResponse {
                comms_request_id: existing,
            }),
        ));
    }

    let locale = body
        .locale
        .clone()
        .unwrap_or_else(|| tenant.config.default_locale.clone());

    let template = template_repo::find(
        &tenant.pool,
        &body.template_id,
        body.template_version,
        &locale,
    )
    .await?
    .ok_or(IngestError::TemplateNotFound)?;

    let rendered_body = render(&template.body, &body.variables)?;

    let dek = get_or_create_dek(
        &tenant.pool,
        &*app.keystore,
        &tenant.dek_cache,
        &tenant.tenant.vault_mount,
        body.customer_id,
    )
    .await?;

    let comms_request_id = Uuid::new_v4();
    // Mints its own address_id (T-011 decision 4): no `customer_address`
    // table exists yet (T-015), and this value is stored only in `outbox`,
    // which is deleted on reaching a terminal state well before any future
    // resolution path would need to reconcile it.
    let address_id = Uuid::new_v4();
    let aad = comms_request_id.as_bytes();

    let destination_hmac = destination_hmac::compute(&tenant.pepper, &body.destination);
    let destination_ciphertext =
        encryption::encrypt(&dek, aad, body.destination.as_bytes())?;
    let payload_ciphertext = encryption::encrypt(&dek, aad, rendered_body.as_bytes())?;

    let priority: i16 = if body.class == class::MARKETING { 2 } else { 1 };

    let outcome = insert_transactional(
        &tenant.pool,
        &idempotency_key,
        comms_request_id,
        tenant.tenant.id,
        body.customer_id,
        &body.channel,
        &body.class,
        priority,
        &body.template_id,
        body.template_version,
        body.campaign_id.as_deref(),
        &destination_hmac,
        &destination_ciphertext,
        &payload_ciphertext,
        producer.producer_id,
        address_id,
    )
    .await?;

    match outcome {
        InsertOutcome::Created => Ok((
            StatusCode::CREATED,
            Json(CreateCommsResponse { comms_request_id }),
        )),
        InsertOutcome::Replayed { comms_request_id } => Ok((
            StatusCode::OK,
            Json(CreateCommsResponse { comms_request_id }),
        )),
    }
}

fn validate_channel(value: &str) -> Result<(), IngestError> {
    match value {
        channel::SMS | channel::EMAIL | channel::WHATSAPP => Ok(()),
        other => Err(IngestError::InvalidChannel(other.to_string())),
    }
}

fn validate_class(value: &str, campaign_id: Option<&str>) -> Result<(), IngestError> {
    match value {
        class::TRANSACTIONAL if campaign_id.is_some() => {
            Err(IngestError::CampaignIdOnTransactional)
        }
        class::TRANSACTIONAL | class::MARKETING => Ok(()),
        other => Err(IngestError::InvalidClass(other.to_string())),
    }
}

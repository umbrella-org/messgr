use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use crate::customer::resolve::{ResolutionInput, resolve};
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
use super::repo::{CancelOutcome, InsertOutcome, insert_transactional};

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
    let resolution_input = build_resolution_input(&body)?;
    validate_schedule(
        body.scheduled_for,
        producer.tenant.config.schedule_horizon_days,
    )?;

    let tenant = producer.tenant;

    // Fast pre-check: skip resolution/template/DEK/encryption work for a
    // genuine retry. A retry of an already-accepted request must still
    // replay even under an engaged switch — the row already exists; dispatch
    // holding it is dispatch's concern, not a reason to reject the retry.
    if let Some(existing) = super::repo::find_idempotent_reply(
        &tenant.pool,
        producer.producer_id,
        &idempotency_key,
    )
    .await?
    {
        return Ok((
            StatusCode::OK,
            Json(CreateCommsResponse {
                comms_request_id: existing,
            }),
        ));
    }

    // DESIGN.md §5.2, T-016 decision 6: checked before any resolution/
    // template/DEK work, against the tenant's own poll-refreshed cache
    // (`LISTEN` never fires over this process's PgBouncer connection, §2.3).
    if let Some(scope) = tenant
        .kill_switches
        .blocking_scope(
            &body.channel,
            producer.producer_id,
            body.campaign_id.as_deref(),
        )
        .await
    {
        return Err(IngestError::KillSwitchEngaged { scope });
    }

    let resolved = resolve(
        &tenant.pool,
        &*app.keystore,
        &tenant.dek_cache,
        &tenant.tenant.vault_mount,
        &tenant.pepper,
        resolution_input,
        &body.destination,
        &body.channel,
        &tenant.config.default_locale,
        &tenant.config.default_timezone,
    )
    .await?;

    let locale = body
        .locale
        .clone()
        .or_else(|| resolved.locale.clone())
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
        resolved.customer_id,
    )
    .await?;

    let comms_request_id = Uuid::new_v4();
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
        resolved.customer_id,
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
        resolved.address_id,
        body.scheduled_for,
        body.expires_at,
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

/// DESIGN.md §6.2, T-041: producer-scoped cancellation. `404` for an unknown
/// id or one belonging to another producer (decision 1, no ownership
/// disclosure); `409` when it already reached a non-cancelled terminal
/// state; `204` idempotently otherwise.
pub async fn cancel_comms(
    State(_app): State<AppState>,
    producer: ProducerContext,
    Path(comms_request_id): Path<Uuid>,
) -> Result<StatusCode, IngestError> {
    match super::repo::cancel(
        &producer.tenant.pool,
        comms_request_id,
        producer.producer_id,
    )
    .await?
    {
        CancelOutcome::Cancelled => Ok(StatusCode::NO_CONTENT),
        CancelOutcome::AlreadySent => Err(IngestError::AlreadySent),
        CancelOutcome::NotFound => Err(IngestError::CommsRequestNotFound),
    }
}

/// Decision 4: exactly one of three shapes is legal — `customer_id` alone,
/// `external_id` + `external_id_system` together, or all three unset
/// (address-only resolution via `destination`). Any other combination is
/// rejected before any DB or Vault call.
fn build_resolution_input(
    body: &CreateCommsRequest,
) -> Result<ResolutionInput, IngestError> {
    match (
        &body.customer_id,
        &body.external_id,
        &body.external_id_system,
    ) {
        (Some(id), None, None) => Ok(ResolutionInput::Explicit(*id)),
        (None, Some(external_id), Some(system)) => Ok(ResolutionInput::External {
            system: system.clone(),
            external_id: external_id.clone(),
        }),
        (None, None, None) => Ok(ResolutionInput::AddressOnly),
        _ => Err(IngestError::InvalidResolutionInput),
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

/// T-040 decision 1: only a `scheduled_for` beyond the tenant's horizon is
/// rejected; a past one collapses to "send immediately" via
/// `next_attempt_at = scheduled_for.unwrap_or(now)`, no special-casing here.
fn validate_schedule(
    scheduled_for: Option<DateTime<Utc>>,
    horizon_days: i32,
) -> Result<(), IngestError> {
    if let Some(scheduled_for) = scheduled_for
        && scheduled_for > Utc::now() + Duration::days(horizon_days.into())
    {
        return Err(IngestError::ScheduleHorizonExceeded { horizon_days });
    }
    Ok(())
}

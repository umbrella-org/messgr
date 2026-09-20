use std::str::FromStr;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{FromRequestParts, MatchedPath, Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, DurationRound, TimeDelta, TimeZone, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use uuid::Uuid;

use crate::access_audit;
use crate::auth::role;
use crate::comms_query::{
    filter::CommsFilter, model::CommsRequestDetailView, repo as comms_repo,
};
use crate::customer_dek::repo as customer_dek_repo;
use crate::encryption;
use crate::producer_quota::repo as producer_quota_repo;
use crate::tenant_config::repo as tenant_config_repo;

use super::AppState;
use super::auth_mw::{AuthedUser, require_role};
use super::path::{CampaignIdPath, IdPath};
use super::tenant::TenantContext;

/// Rows returned by `GET /comms` and `GET /customers/{id}/timeline` are
/// capped at this many — a first-cut fixed limit, not a paginated cursor
/// (T-048 Task 5/6: no pagination scheme was specified).
const LIST_LIMIT: i64 = 200;

pub(crate) fn database_error(err: sqlx::Error) -> Response {
    tracing::error!(%err, "query-api: database error");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

// ---------------------------------------------------------------------
// Data routes
// ---------------------------------------------------------------------

pub async fn list_comms(
    AuthedUser(identity): AuthedUser,
    tenant: TenantContext,
    Query(filter): Query<CommsFilter>,
) -> Response {
    if let Err(status) = require_role(&identity, &[role::COMPLIANCE]) {
        return status.into_response();
    }

    match comms_repo::list(&tenant.pool, &filter, LIST_LIMIT).await {
        Ok(rows) => Json(rows).into_response(),
        Err(err) => database_error(err),
    }
}

#[derive(Debug, Deserialize)]
pub struct CommsDetailQuery {
    created_at: DateTime<Utc>,
}

pub async fn comms_detail(
    AuthedUser(identity): AuthedUser,
    tenant: TenantContext,
    Path(IdPath { id }): Path<IdPath>,
    Query(query): Query<CommsDetailQuery>,
    State(state): State<AppState>,
) -> Response {
    let row = match comms_repo::detail(&tenant.pool, query.created_at, id).await {
        Ok(Some(row)) => row,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => return database_error(err),
    };

    // customer_service is restricted here, inline (T-048 decision 12,
    // corrected after F5: shipped as a per-handler check reusing the shared
    // `identity_owns` predicate, not a `FromRequestParts` extractor): it
    // must own this row's customer, or 403 before any further work.
    match identity.role.as_str() {
        role::COMPLIANCE => {}
        role::CUSTOMER_SERVICE if identity_owns(&identity, row.customer_id) => {}
        role::CUSTOMER_SERVICE => return StatusCode::FORBIDDEN.into_response(),
        _ => return StatusCode::FORBIDDEN.into_response(),
    }

    match decrypt_detail(&state, &tenant, row).await {
        Ok(view) => Json(view).into_response(),
        Err(status) => status.into_response(),
    }
}

/// `customer_service`'s `Identity.actor` doubles as the customer_id it may
/// look up, since `MockProvider` has no session concept beyond process
/// config (T-048 decision 2/12) — the actor string is parsed as a `Uuid` and
/// compared to the row's owner. An unparseable actor never matches, so it
/// fails closed to `403`.
pub(crate) fn identity_owns(
    identity: &crate::auth::provider::Identity,
    customer_id: Uuid,
) -> bool {
    Uuid::parse_str(&identity.actor)
        .map(|owned| owned == customer_id)
        .unwrap_or(false)
}

/// DEK find -> Vault unwrap -> decrypt (T-048 decision 14). A missing
/// `customer_dek` row is a data-integrity error, not a fallback case: a
/// `comms_request` row only exists because ingest already created one.
pub(crate) async fn decrypt_detail(
    state: &AppState,
    tenant: &TenantContext,
    row: crate::comms_query::model::CommsRequestDetail,
) -> Result<CommsRequestDetailView, StatusCode> {
    let dek_row = customer_dek_repo::find(&tenant.pool, row.customer_id)
        .await
        .map_err(|err| {
            tracing::error!(%err, "query-api: database error loading customer_dek");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or_else(|| {
            tracing::error!(
                customer_id = %row.customer_id,
                "query-api: comms_request references a customer with no customer_dek row"
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let dek = state
        .keystore
        .unwrap_dek(&tenant.vault_mount, &dek_row.wrapped_dek)
        .await
        .map_err(|err| {
            tracing::error!(%err, "query-api: Vault unwrap failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let aad = row.id.as_bytes();
    let destination_bytes = encryption::decrypt(&dek, aad, &row.destination_ciphertext)
        .map_err(|err| {
            tracing::error!(%err, "query-api: destination decrypt failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let destination = String::from_utf8(destination_bytes).map_err(|err| {
        tracing::error!(%err, "query-api: decrypted destination was not valid utf8");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let body = match &row.payload_ciphertext {
        Some(ciphertext) => {
            let bytes = encryption::decrypt(&dek, aad, ciphertext).map_err(|err| {
                tracing::error!(%err, "query-api: payload decrypt failed");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
            Some(String::from_utf8(bytes).map_err(|err| {
                tracing::error!(%err, "query-api: decrypted payload was not valid utf8");
                StatusCode::INTERNAL_SERVER_ERROR
            })?)
        }
        None => None,
    };

    Ok(CommsRequestDetailView {
        id: row.id,
        created_at: row.created_at,
        customer_id: row.customer_id,
        channel: row.channel,
        class: row.class,
        template_id: row.template_id,
        template_version: row.template_version,
        campaign_id: row.campaign_id,
        destination,
        body,
        producer_id: row.producer_id,
        scheduled_for: row.scheduled_for,
        expires_at: row.expires_at,
        final_status: row.final_status,
        finalized_at: row.finalized_at,
    })
}

pub async fn customer_timeline(
    AuthedUser(identity): AuthedUser,
    tenant: TenantContext,
    Path(IdPath { id: customer_id }): Path<IdPath>,
) -> Response {
    match identity.role.as_str() {
        role::COMPLIANCE => {}
        role::CUSTOMER_SERVICE if identity_owns(&identity, customer_id) => {}
        _ => return StatusCode::FORBIDDEN.into_response(),
    }

    match comms_repo::timeline(&tenant.pool, customer_id, LIST_LIMIT).await {
        Ok(rows) => Json(rows).into_response(),
        Err(err) => database_error(err),
    }
}

pub async fn campaign_reach(
    AuthedUser(identity): AuthedUser,
    tenant: TenantContext,
    Path(CampaignIdPath { id: campaign_id }): Path<CampaignIdPath>,
) -> Response {
    if let Err(status) =
        require_role(&identity, &[role::COMPLIANCE, role::CAMPAIGN_OPS])
    {
        return status.into_response();
    }

    match comms_repo::campaign_reach(&tenant.pool, &campaign_id).await {
        Ok(counts) => Json(counts).into_response(),
        Err(err) => database_error(err),
    }
}

/// `now.duration_trunc(TimeDelta::minutes(1))` — mirrors
/// `producer_quota::tracker::QuotaTracker::minute_window_start` exactly
/// (that one is private to the tracker, and this read path has no
/// `QuotaTracker` instance to borrow it from).
fn minute_window_start(now: DateTime<Utc>) -> DateTime<Utc> {
    now.duration_trunc(TimeDelta::minutes(1))
        .expect("minute truncation is infallible")
}

/// Local midnight in `tz`, mapped back to UTC — mirrors
/// `QuotaTracker::day_window_start`/`resolve_local_midnight`'s ambiguous/
/// nonexistent handling. `ponytail: duplicated rather than exposed from
/// tracker.rs (private there); if this drifts from the dispatcher's own
/// windowing, extract a shared helper instead.`
fn day_window_start(tz: Tz, now: DateTime<Utc>) -> DateTime<Utc> {
    let local_date = now.with_timezone(&tz).date_naive();
    let naive_midnight = local_date
        .and_hms_opt(0, 0, 0)
        .expect("midnight is always a valid time-of-day");
    match tz.from_local_datetime(&naive_midnight).earliest() {
        Some(dt) => dt.with_timezone(&Utc),
        None => tz.from_utc_datetime(&naive_midnight).with_timezone(&Utc),
    }
}

/// Matches `dispatcher.rs`'s own fallback for a tenant with no
/// `tenant_config` row at all (T-007 decision 4: no auto-seeding).
const DEFAULT_QUOTA_DAY_BOUNDARY_TZ: &str = "UTC";

async fn current_windows(
    pool: &sqlx::PgPool,
) -> Result<(DateTime<Utc>, DateTime<Utc>), sqlx::Error> {
    let tz_name = tenant_config_repo::load(pool)
        .await?
        .map(|config| config.quota_day_boundary_tz)
        .unwrap_or_else(|| DEFAULT_QUOTA_DAY_BOUNDARY_TZ.to_string());
    let tz = Tz::from_str(&tz_name).unwrap_or(Tz::UTC);

    let now = Utc::now();
    Ok((minute_window_start(now), day_window_start(tz, now)))
}

pub async fn producer_usage(
    AuthedUser(identity): AuthedUser,
    tenant: TenantContext,
    Path(IdPath { id: producer_id }): Path<IdPath>,
) -> Response {
    if let Err(status) = require_role(&identity, &[role::COMMS_OPS]) {
        return status.into_response();
    }

    let (minute_start, day_start) = match current_windows(&tenant.pool).await {
        Ok(windows) => windows,
        Err(err) => return database_error(err),
    };

    match producer_quota_repo::load_current_usage(&tenant.pool, minute_start, day_start)
        .await
    {
        Ok(rows) => {
            let mine: Vec<_> = rows
                .into_iter()
                .filter(|row| row.producer_id == producer_id)
                .collect();
            Json(mine).into_response()
        }
        Err(err) => database_error(err),
    }
}

#[derive(Debug, Deserialize)]
pub struct ProducerQuotaQuery {
    channel: String,
    class: String,
}

pub async fn producer_quota(
    AuthedUser(identity): AuthedUser,
    tenant: TenantContext,
    Path(IdPath { id: producer_id }): Path<IdPath>,
    Query(query): Query<ProducerQuotaQuery>,
) -> Response {
    if let Err(status) = require_role(&identity, &[role::COMMS_OPS]) {
        return status.into_response();
    }

    match producer_quota_repo::load_one(
        &tenant.pool,
        producer_id,
        &query.channel,
        &query.class,
    )
    .await
    {
        Ok(Some(quota)) => Json(quota).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(err) => database_error(err),
    }
}

// ---------------------------------------------------------------------
// Unauthenticated static routes (decisions 9/10)
// ---------------------------------------------------------------------

pub async fn openapi_spec() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/yaml")],
        include_str!("../../openapi/query-api.yaml"),
    )
}

pub async fn htmx_asset() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/javascript")],
        Bytes::from_static(include_bytes!("../../assets/htmx.min.js")),
    )
}

// ---------------------------------------------------------------------
// Compliance access-audit middleware (decision 17)
// ---------------------------------------------------------------------

/// The one route whose own path segment names a `customer_id` directly —
/// every other route's `{id}` names something else (a `comms_request` id,
/// a `campaign_id`, a `producer_id`).
const CUSTOMER_ID_ROUTES: &[&str] = &[
    "/t/{tenant_slug}/customers/{id}/timeline",
    "/t/{tenant_slug}/ui/customers/{id}/timeline",
];

pub async fn access_audit_mw(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let matched_path = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_string());
    let query_params = req.uri().query().unwrap_or("").to_string();

    let (mut parts, body) = req.into_parts();
    // Re-resolves tenant + identity a second time (the handler's own
    // `AuthedUser`/`TenantContext` extractors do it again) — cheap for
    // `MockProvider` (no network) and a cached pool lookup (T-048 decision
    // 4); not worth a shared-extension cache for a first cut.
    let tenant_ctx = match TenantContext::from_request_parts(&mut parts, &state).await {
        Ok(tenant_ctx) => tenant_ctx,
        Err(rejection) => return rejection.into_response(),
    };
    let identity = match state.auth.authenticate(tenant_ctx.tenant_id).await {
        Ok(identity) => identity,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let req = Request::from_parts(parts, body);

    if identity.role == role::COMPLIANCE {
        let customer_id = matched_path.as_deref().and_then(|path| {
            if CUSTOMER_ID_ROUTES.contains(&path) {
                extract_path_customer_id(req.uri().path())
            } else {
                None
            }
        });

        // access_audit lives in the tenant's own database (migration 0019),
        // not the control database.
        if let Err(err) = access_audit::repo::record(
            &tenant_ctx.pool,
            &access_audit::model::AccessAuditInput {
                actor: identity.actor.clone(),
                role: identity.role.clone(),
                route: matched_path.clone().unwrap_or_default(),
                customer_id,
                query_params,
            },
            Utc::now(),
        )
        .await
        {
            tracing::error!(%err, "query-api: failed to write access_audit row");
        }
    }

    next.run(req).await
}

/// Pulls the final path segment before `/timeline` and parses it as a
/// `Uuid` — good enough for the one route shape this is ever called against
/// (`CUSTOMER_ID_ROUTES`).
fn extract_path_customer_id(path: &str) -> Option<Uuid> {
    let segments: Vec<&str> = path.split('/').collect();
    let idx = segments.iter().position(|s| *s == "timeline")?;
    segments
        .get(idx.checked_sub(1)?)
        .and_then(|s| Uuid::parse_str(s).ok())
}

// ---------------------------------------------------------------------
// UI handlers (Task 7)
// ---------------------------------------------------------------------

pub use super::views::{ui_campaign_reach, ui_comms_detail, ui_customer_timeline};

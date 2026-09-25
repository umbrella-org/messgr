//! The admin panel (T-049): quota dashboard, kill-switch console, scheduled
//! queue, producer registry. Mirrors `views.rs`/`handlers.rs`'s
//! `AuthedUser`/`TenantContext`/`require_role`/`render()` pattern (decision
//! 1) — a route gated to one role is `403` for the other, never a superset.
//!
//! Mutation handlers call the tenant-pool-taking `*_inner`/`repo::*`
//! functions directly (decision 3 and its pre-pickup correction), not the
//! CLI-shaped wrappers in `producer::register`/`producer_quota::configure`
//! that open and close their own tenant pool per call — this panel already
//! has the tenant's pool open via `TenantContext`.

use std::collections::HashMap;

use askama::Template;
use axum::extract::{Form, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::role;
use crate::ingest::model::class;
use crate::ingest::repo as ingest_repo;
use crate::kill_switch::configure as kill_switch_configure;
use crate::kill_switch::model::KillSwitch;
use crate::kill_switch::repo as kill_switch_repo;
use crate::outbox_query::filter::OutboxQueueFilter;
use crate::outbox_query::model::OutboxQueueRow;
use crate::outbox_query::repo as outbox_query_repo;
use crate::platform_kill_switch::repo as platform_kill_switch_repo;
use crate::producer::model::Producer;
use crate::producer::register as producer_register;
use crate::producer::repo as producer_repo;
use crate::producer_quota::configure as producer_quota_configure;
use crate::producer_quota::model::{
    ProducerQuotaInput, ProducerQuotaOverrideInput, enforcement,
};
use crate::producer_quota::repo as producer_quota_repo;

use super::AppState;
use super::auth_mw::{AuthedUser, require_role};
use super::handlers::database_error;
use super::path::{IdPath, TenantSlugPath};
use super::render;
use super::tenant::TenantContext;

/// Same cap as `handlers::LIST_LIMIT` — a first-cut fixed limit, no
/// pagination scheme specified for this panel either.
const SCHEDULED_LIMIT: i64 = 200;

/// `sqlx::Error::Configuration` is this codebase's established "domain
/// rejection, not a database failure" sentinel (every `*Error::Database`
/// wrapper in `producer::register`/`producer_quota::configure`/
/// `kill_switch::configure` uses it the same way) — surfaced here as `400`,
/// never the raw `500` a database error gets.
fn database_or_rejected(err: sqlx::Error) -> Response {
    if matches!(err, sqlx::Error::Configuration(_)) {
        (StatusCode::BAD_REQUEST, err.to_string()).into_response()
    } else {
        database_error(err)
    }
}

fn producer_name_map(producers: &[Producer]) -> HashMap<Uuid, String> {
    producers.iter().map(|p| (p.id, p.name.clone())).collect()
}

fn producer_name(names: &HashMap<Uuid, String>, id: Uuid) -> String {
    names.get(&id).cloned().unwrap_or_else(|| id.to_string())
}

fn format_limit(value: i64, limit: Option<i32>) -> String {
    match limit {
        Some(limit) => format!("{value} / {limit}"),
        None => format!("{value} (no limit)"),
    }
}

fn format_optional_limit(limit: Option<i32>) -> String {
    match limit {
        Some(limit) => limit.to_string(),
        None => "no limit".to_string(),
    }
}

/// Trailing-24h sent-volume points for the quota dashboard's inline SVG
/// sparkline (Task 7) — normalized into a 240x40 viewBox. An empty series
/// renders no polyline at all (Askama prints an empty `points` attribute).
fn build_sparkline(values: &[i64]) -> String {
    if values.is_empty() {
        return String::new();
    }
    let max = values.iter().copied().max().unwrap_or(0).max(1) as f64;
    let step = if values.len() > 1 {
        240.0 / (values.len() - 1) as f64
    } else {
        0.0
    };
    values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = i as f64 * step;
            let y = 40.0 - (*v as f64 / max) * 40.0;
            format!("{x:.1},{y:.1}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// An HTML `datetime-local` input's value (`YYYY-MM-DDTHH:MM`, no timezone)
/// is not valid RFC3339 — parsed as naive and treated as UTC (no timezone
/// picker in this minimal form surface, decision 8). Falls back to RFC3339
/// first so a non-browser client can still send a fully-qualified timestamp.
fn parse_form_datetime(value: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Some(dt.with_timezone(&Utc));
    }
    NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M")
        .ok()
        .map(|naive| naive.and_utc())
}

// ---------------------------------------------------------------------
// Quota dashboard (comms_ops, view only)
// ---------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/quota.html")]
struct QuotaTemplate {
    actor: String,
    role: String,
    tenant_slug: String,
    rows: Vec<QuotaRow>,
}

struct QuotaRow {
    producer_name: String,
    channel: String,
    class: String,
    minute_display: String,
    day_display: String,
    blocked_24h: i64,
    sparkline_points: String,
}

pub async fn ui_quota_dashboard(
    AuthedUser(identity): AuthedUser,
    tenant: TenantContext,
    Path(TenantSlugPath { tenant_slug }): Path<TenantSlugPath>,
) -> Response {
    if let Err(status) = require_role(&identity, &[role::COMMS_OPS]) {
        return status.into_response();
    }

    let quotas = match producer_quota_repo::list(&tenant.pool).await {
        Ok(rows) => rows,
        Err(err) => return database_error(err),
    };
    let producers = match producer_repo::list(&tenant.pool).await {
        Ok(rows) => rows,
        Err(err) => return database_error(err),
    };
    let names = producer_name_map(&producers);

    let (minute_start, day_start) =
        match super::handlers::current_windows(&tenant.pool).await {
            Ok(windows) => windows,
            Err(err) => return database_error(err),
        };
    let current = match producer_quota_repo::load_current_usage(
        &tenant.pool,
        minute_start,
        day_start,
    )
    .await
    {
        Ok(rows) => rows,
        Err(err) => return database_error(err),
    };
    let since = Utc::now() - Duration::hours(24);
    let history =
        match producer_quota_repo::load_usage_history(&tenant.pool, since).await {
            Ok(rows) => rows,
            Err(err) => return database_error(err),
        };

    let rows = quotas
        .into_iter()
        .map(|q| {
            let matches_key = |u: &&crate::producer_quota::model::UsageRow| {
                u.producer_id == q.producer_id
                    && u.channel == q.channel
                    && u.class == q.class
            };
            let minute_sent = current
                .iter()
                .filter(matches_key)
                .find(|u| u.granularity == "minute")
                .map(|u| u.sent)
                .unwrap_or(0);
            let day_sent = current
                .iter()
                .filter(matches_key)
                .find(|u| u.granularity == "day")
                .map(|u| u.sent)
                .unwrap_or(0);
            let key_history: Vec<&crate::producer_quota::model::UsageRow> =
                history.iter().filter(matches_key).collect();
            let sparkline: Vec<i64> = key_history.iter().map(|u| u.sent).collect();
            let blocked_24h: i64 = key_history.iter().map(|u| u.blocked).sum();

            QuotaRow {
                producer_name: producer_name(&names, q.producer_id),
                channel: q.channel,
                class: q.class,
                minute_display: format_limit(minute_sent, q.per_minute),
                day_display: format_limit(day_sent, q.per_day),
                blocked_24h,
                sparkline_points: build_sparkline(&sparkline),
            }
        })
        .collect();

    render(QuotaTemplate {
        actor: identity.actor,
        role: identity.role,
        tenant_slug,
        rows,
    })
}

// ---------------------------------------------------------------------
// Kill-switch console (comms_ops)
// ---------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/kill_switches.html")]
struct KillSwitchesTemplate {
    actor: String,
    role: String,
    tenant_slug: String,
    active: Vec<KillSwitch>,
    /// Active platform-tier switches blocking this tenant (T-058) -- shown
    /// as a distinct, non-actionable suspension block, never as rows with a
    /// release button. Only the engage time is carried to the template: the
    /// operator's reason is provider-internal (T-058's generic-label
    /// decision).
    platform_suspended_since: Vec<DateTime<Utc>>,
}

#[derive(Template)]
#[template(path = "admin/_blast_radius.html")]
struct BlastRadiusTemplate {
    rows: Vec<(String, i64)>,
}

/// Shared by the view and by the engage/release handlers, so the platform
/// suspension block (T-058) is present on every render of this page, not
/// only on a fresh load.
async fn kill_switches_page(
    pool: &PgPool,
    control_pool: &PgPool,
    tenant_id: Uuid,
    actor: String,
    role: String,
    tenant_slug: String,
) -> Response {
    let platform_suspended_since =
        match platform_kill_switch_repo::list_active_for_tenant(control_pool, tenant_id)
            .await
        {
            Ok(rows) => rows.into_iter().map(|row| row.engaged_at).collect(),
            Err(err) => return database_error(err),
        };
    match kill_switch_repo::list_active(pool).await {
        Ok(active) => render(KillSwitchesTemplate {
            platform_suspended_since,
            actor,
            role,
            tenant_slug,
            active,
        }),
        Err(err) => database_error(err),
    }
}

pub async fn ui_kill_switches(
    AuthedUser(identity): AuthedUser,
    tenant: TenantContext,
    Path(TenantSlugPath { tenant_slug }): Path<TenantSlugPath>,
    State(state): State<AppState>,
) -> Response {
    if let Err(status) = require_role(&identity, &[role::COMMS_OPS]) {
        return status.into_response();
    }
    kill_switches_page(
        &tenant.pool,
        &state.control_pool,
        tenant.tenant_id,
        identity.actor,
        identity.role,
        tenant_slug,
    )
    .await
}

#[derive(Debug, Deserialize)]
pub struct BlastRadiusQuery {
    scope: String,
    #[serde(default)]
    scope_key: Option<String>,
}

pub async fn ui_blast_radius(
    AuthedUser(identity): AuthedUser,
    tenant: TenantContext,
    Query(query): Query<BlastRadiusQuery>,
) -> Response {
    if let Err(status) = require_role(&identity, &[role::COMMS_OPS]) {
        return status.into_response();
    }
    let scope_key = query.scope_key.filter(|s| !s.is_empty());
    match kill_switch_repo::blast_radius(
        &tenant.pool,
        &query.scope,
        scope_key.as_deref(),
    )
    .await
    {
        Ok(rows) => render(BlastRadiusTemplate { rows }),
        Err(err) => database_error(err),
    }
}

#[derive(Debug, Deserialize)]
pub struct EngageForm {
    scope: String,
    #[serde(default)]
    scope_key: Option<String>,
    on_queued: String,
    reason: String,
}

pub async fn engage_kill_switch(
    AuthedUser(identity): AuthedUser,
    tenant: TenantContext,
    Path(TenantSlugPath { tenant_slug }): Path<TenantSlugPath>,
    State(state): State<AppState>,
    Form(form): Form<EngageForm>,
) -> Response {
    if let Err(status) = require_role(&identity, &[role::COMMS_OPS]) {
        return status.into_response();
    }
    let scope_key = form.scope_key.filter(|s| !s.is_empty());

    let result = kill_switch_configure::engage(
        &tenant.pool,
        &state.control_pool,
        tenant.tenant_id,
        &form.scope,
        scope_key.as_deref(),
        &form.on_queued,
        &form.reason,
        &identity.actor,
    )
    .await;

    match result {
        Ok(_) => {
            kill_switches_page(
                &tenant.pool,
                &state.control_pool,
                tenant.tenant_id,
                identity.actor,
                identity.role,
                tenant_slug,
            )
            .await
        }
        Err(kill_switch_configure::ConfigureError::AlreadyEngaged) => {
            StatusCode::CONFLICT.into_response()
        }
        Err(kill_switch_configure::ConfigureError::Database(err)) => {
            database_or_rejected(err)
        }
    }
}

pub async fn release_kill_switch(
    AuthedUser(identity): AuthedUser,
    tenant: TenantContext,
    Path(IdPath { id }): Path<IdPath>,
    Path(TenantSlugPath { tenant_slug }): Path<TenantSlugPath>,
    State(state): State<AppState>,
) -> Response {
    if let Err(status) = require_role(&identity, &[role::COMMS_OPS]) {
        return status.into_response();
    }

    match kill_switch_configure::release(
        &tenant.pool,
        &state.control_pool,
        tenant.tenant_id,
        id,
        &identity.actor,
    )
    .await
    {
        Ok(_) => {
            kill_switches_page(
                &tenant.pool,
                &state.control_pool,
                tenant.tenant_id,
                identity.actor,
                identity.role,
                tenant_slug,
            )
            .await
        }
        Err(kill_switch_configure::ConfigureError::Database(err)) => {
            database_or_rejected(err)
        }
        Err(kill_switch_configure::ConfigureError::AlreadyEngaged) => {
            unreachable!("release never returns AlreadyEngaged")
        }
    }
}

// ---------------------------------------------------------------------
// Scheduled queue (comms_ops)
// ---------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/scheduled.html")]
struct ScheduledTemplate {
    actor: String,
    role: String,
    tenant_slug: String,
    filter_producer_id: String,
    filter_campaign_id: String,
    rows: Vec<OutboxQueueRow>,
}

/// Looser than `OutboxQueueFilter` itself (Task 3, decision 6, pinned
/// exactly): tolerates a present-but-empty `producer_id`/`campaign_id`,
/// which is what the admin panel's own filter form sends once its bound
/// signal is cleared back to `''` rather than omitting the query param
/// entirely. `OutboxQueueFilter` itself stays exactly as decision 6
/// specifies it — this is an HTTP-boundary adapter, not a change to it.
#[derive(Debug, Deserialize)]
pub struct ScheduledQuery {
    #[serde(default, deserialize_with = "empty_uuid_as_none")]
    producer_id: Option<Uuid>,
    #[serde(default)]
    campaign_id: Option<String>,
}

fn empty_uuid_as_none<'de, D>(deserializer: D) -> Result<Option<Uuid>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    match opt.as_deref() {
        None | Some("") => Ok(None),
        Some(s) => Uuid::parse_str(s)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

impl From<ScheduledQuery> for OutboxQueueFilter {
    fn from(query: ScheduledQuery) -> Self {
        OutboxQueueFilter {
            producer_id: query.producer_id,
            campaign_id: query.campaign_id.filter(|s| !s.is_empty()),
            due_before: None,
            due_after: None,
        }
    }
}

async fn scheduled_page(
    pool: &PgPool,
    actor: String,
    role: String,
    tenant_slug: String,
    filter: OutboxQueueFilter,
) -> Response {
    let filter_producer_id = filter
        .producer_id
        .map(|id| id.to_string())
        .unwrap_or_default();
    let filter_campaign_id = filter.campaign_id.clone().unwrap_or_default();

    match outbox_query_repo::list(pool, &filter, SCHEDULED_LIMIT).await {
        Ok(rows) => render(ScheduledTemplate {
            actor,
            role,
            tenant_slug,
            filter_producer_id,
            filter_campaign_id,
            rows,
        }),
        Err(err) => database_error(err),
    }
}

pub async fn ui_scheduled_queue(
    AuthedUser(identity): AuthedUser,
    tenant: TenantContext,
    Path(TenantSlugPath { tenant_slug }): Path<TenantSlugPath>,
    Query(query): Query<ScheduledQuery>,
) -> Response {
    if let Err(status) = require_role(&identity, &[role::COMMS_OPS]) {
        return status.into_response();
    }
    scheduled_page(
        &tenant.pool,
        identity.actor,
        identity.role,
        tenant_slug,
        query.into(),
    )
    .await
}

#[derive(Debug, Deserialize)]
pub struct CancelForm {
    producer_id: Uuid,
}

pub async fn cancel_scheduled(
    AuthedUser(identity): AuthedUser,
    tenant: TenantContext,
    Path(IdPath { id }): Path<IdPath>,
    Path(TenantSlugPath { tenant_slug }): Path<TenantSlugPath>,
    Form(form): Form<CancelForm>,
) -> Response {
    if let Err(status) = require_role(&identity, &[role::COMMS_OPS]) {
        return status.into_response();
    }

    match ingest_repo::cancel(&tenant.pool, id, form.producer_id).await {
        // Cancelled, AlreadySent, and NotFound are all a safe no-op response
        // from this route's perspective (T-041's own idempotent shape) — the
        // list this re-renders simply reflects whatever the row's state
        // actually is now.
        Ok(_) => {
            scheduled_page(
                &tenant.pool,
                identity.actor,
                identity.role,
                tenant_slug,
                OutboxQueueFilter::default(),
            )
            .await
        }
        Err(err) => database_error(err),
    }
}

// ---------------------------------------------------------------------
// Producer registry (admin)
// ---------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/producers.html")]
struct ProducersTemplate {
    actor: String,
    role: String,
    tenant_slug: String,
    producers: Vec<Producer>,
    quotas: Vec<QuotaDisplayRow>,
    overrides: Vec<OverrideDisplayRow>,
}

struct QuotaDisplayRow {
    producer_name: String,
    channel: String,
    class: String,
    per_minute_display: String,
    per_day_display: String,
    enforcement: String,
}

struct OverrideDisplayRow {
    producer_name: String,
    channel: String,
    class: String,
    per_day: i32,
    valid_from: DateTime<Utc>,
    valid_to: DateTime<Utc>,
    approved_by: String,
    reason: String,
}

async fn producers_page(
    pool: &PgPool,
    actor: String,
    role: String,
    tenant_slug: String,
) -> Response {
    let producers = match producer_repo::list(pool).await {
        Ok(rows) => rows,
        Err(err) => return database_error(err),
    };
    let names = producer_name_map(&producers);

    let quotas = match producer_quota_repo::list(pool).await {
        Ok(rows) => rows,
        Err(err) => return database_error(err),
    };
    let overrides = match producer_quota_repo::list_overrides(pool).await {
        Ok(rows) => rows,
        Err(err) => return database_error(err),
    };

    let quota_rows = quotas
        .into_iter()
        .map(|q| QuotaDisplayRow {
            producer_name: producer_name(&names, q.producer_id),
            channel: q.channel,
            class: q.class,
            per_minute_display: format_optional_limit(q.per_minute),
            per_day_display: format_optional_limit(q.per_day),
            enforcement: q.enforcement,
        })
        .collect();

    let override_rows = overrides
        .into_iter()
        .map(|o| OverrideDisplayRow {
            producer_name: producer_name(&names, o.producer_id),
            channel: o.channel,
            class: o.class,
            per_day: o.per_day,
            valid_from: o.valid_from,
            valid_to: o.valid_to,
            approved_by: o.approved_by,
            reason: o.reason,
        })
        .collect();

    render(ProducersTemplate {
        actor,
        role,
        tenant_slug,
        producers,
        quotas: quota_rows,
        overrides: override_rows,
    })
}

pub async fn ui_producer_registry(
    AuthedUser(identity): AuthedUser,
    tenant: TenantContext,
    Path(TenantSlugPath { tenant_slug }): Path<TenantSlugPath>,
) -> Response {
    if let Err(status) = require_role(&identity, &[role::ADMIN]) {
        return status.into_response();
    }
    producers_page(&tenant.pool, identity.actor, identity.role, tenant_slug).await
}

#[derive(Debug, Deserialize)]
pub struct RegisterProducerForm {
    name: String,
    cert_subject: String,
    owner_team: String,
    contact: String,
}

pub async fn register_producer(
    AuthedUser(identity): AuthedUser,
    tenant: TenantContext,
    Path(TenantSlugPath { tenant_slug }): Path<TenantSlugPath>,
    State(state): State<AppState>,
    Form(form): Form<RegisterProducerForm>,
) -> Response {
    if let Err(status) = require_role(&identity, &[role::ADMIN]) {
        return status.into_response();
    }

    let result = producer_register::register_producer_inner(
        &state.control_pool,
        &tenant.pool,
        tenant.tenant_id,
        &form.name,
        &form.cert_subject,
        &form.owner_team,
        &form.contact,
        &identity.actor,
    )
    .await;

    match result {
        Ok(_) => {
            producers_page(&tenant.pool, identity.actor, identity.role, tenant_slug)
                .await
        }
        Err(producer_register::ProducerError::Database(err)) => {
            database_or_rejected(err)
        }
    }
}

pub async fn disable_producer(
    AuthedUser(identity): AuthedUser,
    tenant: TenantContext,
    Path(IdPath { id }): Path<IdPath>,
    Path(TenantSlugPath { tenant_slug }): Path<TenantSlugPath>,
    State(state): State<AppState>,
) -> Response {
    if let Err(status) = require_role(&identity, &[role::ADMIN]) {
        return status.into_response();
    }

    // `disable_producer_inner` (T-005) is name-shaped, like the rest of the
    // CLI-era API; the admin route names a producer by id (`producer::repo`
    // already grew `find_by_id` for exactly this bridge).
    let producer = match producer_repo::find_by_id(&tenant.pool, id).await {
        Ok(Some(producer)) => producer,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => return database_error(err),
    };

    let result = producer_register::disable_producer_inner(
        &state.control_pool,
        &tenant.pool,
        tenant.tenant_id,
        &producer.name,
        &identity.actor,
    )
    .await;

    match result {
        Ok(_) => {
            producers_page(&tenant.pool, identity.actor, identity.role, tenant_slug)
                .await
        }
        Err(producer_register::ProducerError::Database(err)) => {
            database_or_rejected(err)
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SetQuotaForm {
    channel: String,
    class: String,
    #[serde(default)]
    per_minute: Option<String>,
    #[serde(default)]
    per_day: Option<String>,
    enforcement: String,
}

pub async fn set_producer_quota(
    AuthedUser(identity): AuthedUser,
    tenant: TenantContext,
    Path(IdPath { id }): Path<IdPath>,
    Path(TenantSlugPath { tenant_slug }): Path<TenantSlugPath>,
    State(state): State<AppState>,
    Form(form): Form<SetQuotaForm>,
) -> Response {
    if let Err(status) = require_role(&identity, &[role::ADMIN]) {
        return status.into_response();
    }

    let per_minute = match form
        .per_minute
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<i32>())
    {
        Some(Ok(v)) => Some(v),
        Some(Err(_)) => return StatusCode::BAD_REQUEST.into_response(),
        None => None,
    };
    let per_day = match form
        .per_day
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<i32>())
    {
        Some(Ok(v)) => Some(v),
        Some(Err(_)) => return StatusCode::BAD_REQUEST.into_response(),
        None => None,
    };

    // `set_producer_quota_inner` skips the class/enforcement legality check
    // its CLI-shaped wrapper (`set_producer_quota`) normally does before
    // calling it (AGENTS.md hard invariant 5: quota must never block
    // transactional/auth traffic) — replicated here, audited the same way,
    // since decision 3 routes around the wrapper entirely.
    let illegal = form.class == class::AUTH
        || (form.class == class::TRANSACTIONAL
            && form.enforcement == enforcement::HARD);
    if illegal {
        crate::platform_audit::record(
            &state.control_pool,
            &identity.actor,
            "producer_quota.set",
            Some(tenant.tenant_id),
            serde_json::json!({
                "producer_id": id,
                "channel": form.channel,
                "class": form.class,
                "per_minute": per_minute,
                "per_day": per_day,
                "enforcement": form.enforcement,
                "outcome": "rejected",
            }),
        )
        .await
        .ok();
        return StatusCode::BAD_REQUEST.into_response();
    }

    let input = ProducerQuotaInput {
        producer_id: id,
        channel: form.channel,
        class: form.class,
        per_minute,
        per_day,
        enforcement: form.enforcement,
    };

    match producer_quota_configure::set_producer_quota_inner(
        &state.control_pool,
        &tenant.pool,
        tenant.tenant_id,
        &identity.actor,
        input,
    )
    .await
    {
        Ok(_) => {
            producers_page(&tenant.pool, identity.actor, identity.role, tenant_slug)
                .await
        }
        Err(producer_quota_configure::ConfigureError::Database(err)) => {
            database_or_rejected(err)
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct AddOverrideForm {
    channel: String,
    class: String,
    per_day: i32,
    valid_from: String,
    valid_to: String,
    approved_by: String,
    reason: String,
}

pub async fn add_quota_override(
    AuthedUser(identity): AuthedUser,
    tenant: TenantContext,
    Path(IdPath { id }): Path<IdPath>,
    Path(TenantSlugPath { tenant_slug }): Path<TenantSlugPath>,
    State(state): State<AppState>,
    Form(form): Form<AddOverrideForm>,
) -> Response {
    if let Err(status) = require_role(&identity, &[role::ADMIN]) {
        return status.into_response();
    }

    let (Some(valid_from), Some(valid_to)) = (
        parse_form_datetime(&form.valid_from),
        parse_form_datetime(&form.valid_to),
    ) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    // Mirrors `producer_quota::configure::add_producer_quota_override`'s own
    // three rejection checks exactly — that function isn't reused here
    // (decision 3's correction: bypass the CLI-shaped wrapper, which would
    // reopen a tenant pool) so its validation is replicated at the same
    // point instead of silently dropped.
    let rejection = if form.class == class::AUTH {
        Some("producer_quota_override cannot be configured for class \"auth\"")
    } else if valid_to <= valid_from {
        Some("producer_quota_override valid_to must be after valid_from")
    } else if form.per_day <= 0 {
        Some("producer_quota_override per_day must be positive")
    } else {
        None
    };

    if let Some(reason) = rejection {
        crate::platform_audit::record(
            &state.control_pool,
            &identity.actor,
            "producer_quota_override.add",
            Some(tenant.tenant_id),
            serde_json::json!({
                "producer_id": id,
                "channel": form.channel,
                "class": form.class,
                "per_day": form.per_day,
                "valid_from": valid_from,
                "valid_to": valid_to,
                "approved_by": form.approved_by,
                "outcome": "rejected",
            }),
        )
        .await
        .ok();
        return (StatusCode::BAD_REQUEST, reason).into_response();
    }

    let override_id = Uuid::new_v4();
    let input = ProducerQuotaOverrideInput {
        producer_id: id,
        channel: form.channel,
        class: form.class,
        per_day: form.per_day,
        valid_from,
        valid_to,
        approved_by: form.approved_by,
        reason: form.reason,
    };

    if let Err(err) =
        producer_quota_repo::insert_override(&tenant.pool, override_id, &input).await
    {
        return database_error(err);
    }

    crate::platform_audit::record(
        &state.control_pool,
        &identity.actor,
        "producer_quota_override.add",
        Some(tenant.tenant_id),
        serde_json::json!({
            "producer_id": input.producer_id,
            "channel": input.channel,
            "class": input.class,
            "per_day": input.per_day,
            "valid_from": input.valid_from,
            "valid_to": input.valid_to,
            "approved_by": input.approved_by,
            "outcome": "created",
        }),
    )
    .await
    .ok();

    producers_page(&tenant.pool, identity.actor, identity.role, tenant_slug).await
}

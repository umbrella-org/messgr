use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::ingest::model::class;
use crate::producer::repo as producer_repo;
use crate::tenant::pool::connect_tenant_pool;
use crate::tenant::repo as tenant_repo;

use super::model::{
    ProducerQuota, ProducerQuotaInput, ProducerQuotaOverride,
    ProducerQuotaOverrideInput, enforcement,
};
use super::repo;

/// A configure/list run can fail on the control-database side, the
/// tenant-database side, or a domain-level rejection (an unknown
/// `tenant_slug`/`producer_name`, or a request that would let quota block
/// transactional/auth traffic, AGENTS.md hard invariant 5) — the last of
/// these is carried as `sqlx::Error::Configuration`, matching
/// `provider_config::ConfigureError`'s shape.
#[derive(Debug)]
pub enum ConfigureError {
    Database(sqlx::Error),
}

impl std::fmt::Display for ConfigureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(err) => write!(f, "producer_quota operation failed: {err}"),
        }
    }
}

impl std::error::Error for ConfigureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(err) => Some(err),
        }
    }
}

impl From<sqlx::Error> for ConfigureError {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err)
    }
}

fn rejected(message: String) -> ConfigureError {
    sqlx::Error::Configuration(message.into()).into()
}

#[derive(Debug)]
pub struct ConfigureOutcome {
    /// "created" | "updated" | "idempotent" — never "rejected", which is
    /// returned as an `Err` instead.
    pub outcome: &'static str,
}

/// Resolves `tenant_slug` (audited-reject on unknown, T-005/F1), opens its
/// pool, then resolves `producer_name` within it (audited-reject on
/// unknown, same pattern). Returns the tenant id (for auditing on the
/// control pool) and the tenant pool alongside the resolved producer.
async fn resolve_tenant_and_producer(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    producer_name: &str,
    actor: &str,
    action: &str,
) -> Result<(Uuid, PgPool, Uuid), ConfigureError> {
    let tenant = match tenant_repo::find_by_slug(control_pool, tenant_slug).await? {
        Some(tenant) => tenant,
        None => {
            crate::platform_audit::record(
                control_pool,
                actor,
                action,
                None,
                serde_json::json!({ "outcome": "rejected", "reason": "unknown tenant_slug" }),
            )
            .await?;
            return Err(rejected(format!(
                "no tenant registered with slug {tenant_slug:?}"
            )));
        }
    };

    let tenant_pool = connect_tenant_pool(
        control_pool,
        base_db_url,
        tenant.id,
        &tenant.database_name,
        5,
    )
    .await?;

    let producer = match producer_repo::find_by_name(&tenant_pool.pool, producer_name)
        .await?
    {
        Some(producer) => producer,
        None => {
            tenant_pool.pool.close().await;
            crate::platform_audit::record(
                control_pool,
                actor,
                action,
                Some(tenant.id),
                serde_json::json!({ "outcome": "rejected", "reason": "unknown producer_name" }),
            )
            .await?;
            return Err(rejected(format!(
                "no producer registered with name {producer_name:?}"
            )));
        }
    };

    Ok((tenant.id, tenant_pool.pool, producer.id))
}

/// Sets (creating or updating) one `(producer, channel, class)` quota row.
///
/// Rejects (audited) any row for `class = "auth"` at all — exempt-but-counted
/// is unconditional in the dispatcher, so a quota row for auth is
/// meaningless — and `enforcement = "hard"` for `class = "transactional"`
/// (AGENTS.md hard invariant 5: quota must never block transactional or
/// auth traffic).
#[allow(clippy::too_many_arguments)]
pub async fn set_producer_quota(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    producer_name: &str,
    channel: &str,
    class_value: &str,
    per_minute: Option<i32>,
    per_day: Option<i32>,
    enforcement_value: &str,
    actor: &str,
) -> Result<ConfigureOutcome, ConfigureError> {
    let (tenant_id, tenant_pool, producer_id) = resolve_tenant_and_producer(
        control_pool,
        base_db_url,
        tenant_slug,
        producer_name,
        actor,
        "producer_quota.set",
    )
    .await?;

    let illegal = class_value == class::AUTH
        || (class_value == class::TRANSACTIONAL
            && enforcement_value == enforcement::HARD);
    if illegal {
        let reason = if class_value == class::AUTH {
            "producer_quota cannot be configured for class \"auth\" — auth traffic is exempt \
             but always counted unconditionally"
        } else {
            "producer_quota cannot use hard enforcement for class \"transactional\" — \
             transactional traffic must never be blocked by quota"
        };
        audit_set(
            control_pool,
            actor,
            Some(tenant_id),
            Some(producer_id),
            channel,
            class_value,
            per_minute,
            per_day,
            enforcement_value,
            "rejected",
        )
        .await?;
        tenant_pool.close().await;
        return Err(rejected(reason.to_string()));
    }

    let input = ProducerQuotaInput {
        producer_id,
        channel: channel.to_string(),
        class: class_value.to_string(),
        per_minute,
        per_day,
        enforcement: enforcement_value.to_string(),
    };

    let result =
        set_producer_quota_inner(control_pool, &tenant_pool, tenant_id, actor, input)
            .await;

    tenant_pool.close().await;
    result
}

pub(crate) async fn set_producer_quota_inner(
    control_pool: &PgPool,
    tenant_pool: &PgPool,
    tenant_id: Uuid,
    actor: &str,
    input: ProducerQuotaInput,
) -> Result<ConfigureOutcome, ConfigureError> {
    let existing =
        repo::load_one(tenant_pool, input.producer_id, &input.channel, &input.class)
            .await?;

    let outcome = match &existing {
        None => "created",
        Some(existing) if input.matches(existing) => "idempotent",
        Some(_) => "updated",
    };

    if outcome != "idempotent" {
        repo::upsert(tenant_pool, &input).await?;
    }

    audit_set(
        control_pool,
        actor,
        Some(tenant_id),
        Some(input.producer_id),
        &input.channel,
        &input.class,
        input.per_minute,
        input.per_day,
        &input.enforcement,
        outcome,
    )
    .await?;

    Ok(ConfigureOutcome { outcome })
}

/// Lists every `producer_quota` row for a tenant.
pub async fn list_producer_quota(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
) -> Result<Vec<ProducerQuota>, ConfigureError> {
    let tenant = tenant_repo::find_by_slug(control_pool, tenant_slug)
        .await?
        .ok_or_else(|| {
            rejected(format!("no tenant registered with slug {tenant_slug:?}"))
        })?;

    let tenant_pool = connect_tenant_pool(
        control_pool,
        base_db_url,
        tenant.id,
        &tenant.database_name,
        5,
    )
    .await?;
    let rows = repo::list(&tenant_pool.pool).await;
    tenant_pool.pool.close().await;

    Ok(rows?)
}

/// Adds a time-boxed `per_day` uplift. Rejects (audited) `class = "auth"`
/// (same reason as `set_producer_quota`), `valid_to <= valid_from`, or
/// `per_day <= 0`.
#[allow(clippy::too_many_arguments)]
pub async fn add_producer_quota_override(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    producer_name: &str,
    channel: &str,
    class_value: &str,
    per_day: i32,
    valid_from: DateTime<Utc>,
    valid_to: DateTime<Utc>,
    approved_by: &str,
    reason: &str,
    actor: &str,
) -> Result<Uuid, ConfigureError> {
    let (tenant_id, tenant_pool, producer_id) = resolve_tenant_and_producer(
        control_pool,
        base_db_url,
        tenant_slug,
        producer_name,
        actor,
        "producer_quota_override.add",
    )
    .await?;

    let rejection = if class_value == class::AUTH {
        Some("producer_quota_override cannot be configured for class \"auth\"")
    } else if valid_to <= valid_from {
        Some("producer_quota_override valid_to must be after valid_from")
    } else if per_day <= 0 {
        Some("producer_quota_override per_day must be positive")
    } else {
        None
    };
    if let Some(reason) = rejection {
        audit_override_add(
            control_pool,
            actor,
            Some(tenant_id),
            Some(producer_id),
            channel,
            class_value,
            per_day,
            valid_from,
            valid_to,
            approved_by,
            "rejected",
        )
        .await?;
        tenant_pool.close().await;
        return Err(rejected(reason.to_string()));
    }

    let id = Uuid::new_v4();
    let input = ProducerQuotaOverrideInput {
        producer_id,
        channel: channel.to_string(),
        class: class_value.to_string(),
        per_day,
        valid_from,
        valid_to,
        approved_by: approved_by.to_string(),
        reason: reason.to_string(),
    };

    let result = repo::insert_override(&tenant_pool, id, &input).await;
    tenant_pool.close().await;
    result?;

    audit_override_add(
        control_pool,
        actor,
        Some(tenant_id),
        Some(producer_id),
        &input.channel,
        &input.class,
        input.per_day,
        input.valid_from,
        input.valid_to,
        &input.approved_by,
        "created",
    )
    .await?;

    Ok(id)
}

#[allow(clippy::too_many_arguments)]
async fn audit_override_add(
    control_pool: &PgPool,
    actor: &str,
    tenant_id: Option<Uuid>,
    producer_id: Option<Uuid>,
    channel: &str,
    class_value: &str,
    per_day: i32,
    valid_from: DateTime<Utc>,
    valid_to: DateTime<Utc>,
    approved_by: &str,
    outcome: &str,
) -> Result<(), sqlx::Error> {
    crate::platform_audit::record(
        control_pool,
        actor,
        "producer_quota_override.add",
        tenant_id,
        serde_json::json!({
            "producer_id": producer_id,
            "channel": channel,
            "class": class_value,
            "per_day": per_day,
            "valid_from": valid_from,
            "valid_to": valid_to,
            "approved_by": approved_by,
            "outcome": outcome,
        }),
    )
    .await
}

/// Lists every `producer_quota_override` row for a tenant.
pub async fn list_producer_quota_overrides(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
) -> Result<Vec<ProducerQuotaOverride>, ConfigureError> {
    let tenant = tenant_repo::find_by_slug(control_pool, tenant_slug)
        .await?
        .ok_or_else(|| {
            rejected(format!("no tenant registered with slug {tenant_slug:?}"))
        })?;

    let tenant_pool = connect_tenant_pool(
        control_pool,
        base_db_url,
        tenant.id,
        &tenant.database_name,
        5,
    )
    .await?;
    let rows = repo::list_overrides(&tenant_pool.pool).await;
    tenant_pool.pool.close().await;

    Ok(rows?)
}

#[allow(clippy::too_many_arguments)]
async fn audit_set(
    control_pool: &PgPool,
    actor: &str,
    tenant_id: Option<Uuid>,
    producer_id: Option<Uuid>,
    channel: &str,
    class_value: &str,
    per_minute: Option<i32>,
    per_day: Option<i32>,
    enforcement_value: &str,
    outcome: &str,
) -> Result<(), sqlx::Error> {
    crate::platform_audit::record(
        control_pool,
        actor,
        "producer_quota.set",
        tenant_id,
        serde_json::json!({
            "producer_id": producer_id,
            "channel": channel,
            "class": class_value,
            "per_minute": per_minute,
            "per_day": per_day,
            "enforcement": enforcement_value,
            "outcome": outcome,
        }),
    )
    .await
}

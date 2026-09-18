use sqlx::PgPool;

use crate::tenant::pool::connect_tenant_pool;
use crate::tenant::repo as tenant_repo;

use super::model::{QuietHoursPolicy, QuietHoursPolicyInput};
use super::repo;

/// A configure/show run can fail on the control-database side, the
/// tenant-database side, or a domain-level rejection (an unknown
/// `tenant_slug`) — the last of these is carried as
/// `sqlx::Error::Configuration`, matching `tenant_config::configure`'s
/// `ConfigureError` shape.
#[derive(Debug)]
pub enum ConfigureError {
    Database(sqlx::Error),
}

impl std::fmt::Display for ConfigureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(err) => {
                write!(f, "quiet_hours_policy operation failed: {err}")
            }
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

/// Sets (creating or overwriting) the tenant's institution-wide quiet-hours
/// window (T-043 decision 1 — always `scope = 'default'`): resolves
/// `tenant_slug`, opens its pool, loads the existing row (if any), and only
/// writes when the values actually differ — auditing exactly one
/// `quiet_hours_policy.set` `platform_audit` row per call with outcome
/// `created`, `updated`, or `idempotent`.
///
/// An unknown `tenant_slug` audits a `rejected` row (`tenant_id: None`)
/// before returning the error, matching `tenant_config::set_tenant_config`'s
/// T-005/F1 fix.
pub async fn set_quiet_hours_policy(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    input: QuietHoursPolicyInput,
    actor: &str,
) -> Result<ConfigureOutcome, ConfigureError> {
    let tenant = match tenant_repo::find_by_slug(control_pool, tenant_slug).await? {
        Some(tenant) => tenant,
        None => {
            audit(control_pool, actor, None, &input, "rejected").await?;

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

    let result = set_quiet_hours_policy_inner(
        control_pool,
        &tenant_pool.pool,
        tenant.id,
        actor,
        input,
    )
    .await;

    tenant_pool.pool.close().await;
    result
}

async fn set_quiet_hours_policy_inner(
    control_pool: &PgPool,
    tenant_pool: &PgPool,
    tenant_id: uuid::Uuid,
    actor: &str,
    input: QuietHoursPolicyInput,
) -> Result<ConfigureOutcome, ConfigureError> {
    let existing = repo::load_default(tenant_pool).await?;

    let outcome = match &existing {
        None => "created",
        Some(existing) if input.matches(existing) => "idempotent",
        Some(_) => "updated",
    };

    if outcome != "idempotent" {
        repo::upsert_default(tenant_pool, &input).await?;
    }

    audit(control_pool, actor, Some(tenant_id), &input, outcome).await?;

    Ok(ConfigureOutcome { outcome })
}

/// Loads the tenant's quiet-hours window for display (`messgr-control
/// quiet-hours show`). `None` means the tenant has never configured one.
pub async fn show_quiet_hours_policy(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
) -> Result<Option<QuietHoursPolicy>, ConfigureError> {
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
    let policy = repo::load_default(&tenant_pool.pool).await;
    tenant_pool.pool.close().await;

    Ok(policy?)
}

async fn audit(
    control_pool: &PgPool,
    actor: &str,
    tenant_id: Option<uuid::Uuid>,
    input: &QuietHoursPolicyInput,
    outcome: &str,
) -> Result<(), sqlx::Error> {
    crate::platform_audit::record(
        control_pool,
        actor,
        "quiet_hours_policy.set",
        tenant_id,
        serde_json::json!({
            "start_local": input.start_local,
            "end_local": input.end_local,
            "outcome": outcome,
        }),
    )
    .await
}

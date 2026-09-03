use sqlx::PgPool;

use crate::tenant::pool::connect_tenant_pool;
use crate::tenant::repo as tenant_repo;

use super::model::TenantConfigInput;
use super::repo;

/// A configure/show run can fail on the control-database side, the
/// tenant-database side, or a domain-level rejection (an unknown
/// `tenant_slug`) — the last of these is carried as
/// `sqlx::Error::Configuration`, matching `ProducerError`'s shape.
#[derive(Debug)]
pub enum ConfigureError {
    Database(sqlx::Error),
}

impl std::fmt::Display for ConfigureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(err) => write!(f, "tenant_config operation failed: {err}"),
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

/// Sets (creating or overwriting) a tenant's typed configuration (T-007
/// decision 8): resolves `tenant_slug`, opens its pool, loads the existing
/// row (if any), and only writes when the values actually differ — auditing
/// exactly one `tenant_config.set` `platform_audit` row per call with
/// outcome `created`, `updated`, or `idempotent`.
///
/// An unknown `tenant_slug` audits a `rejected` row (`tenant_id: None`)
/// before returning the error — the T-005/F1 review finding showed that
/// skipping the audit on this path is the mistake to avoid, so this ticket
/// gets it right from the start.
pub async fn set_tenant_config(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    input: TenantConfigInput,
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

    let result = set_tenant_config_inner(
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

async fn set_tenant_config_inner(
    control_pool: &PgPool,
    tenant_pool: &PgPool,
    tenant_id: uuid::Uuid,
    actor: &str,
    input: TenantConfigInput,
) -> Result<ConfigureOutcome, ConfigureError> {
    let existing = repo::load(tenant_pool).await?;

    let outcome = match &existing {
        None => "created",
        Some(existing) if input.matches(existing) => "idempotent",
        Some(_) => "updated",
    };

    if outcome != "idempotent" {
        repo::upsert(tenant_pool, &input).await?;
    }

    audit(control_pool, actor, Some(tenant_id), &input, outcome).await?;

    Ok(ConfigureOutcome { outcome })
}

/// Loads a tenant's typed configuration for display (`messgr-control
/// tenant-config show`). `None` means the tenant has never been configured.
pub async fn show_tenant_config(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
) -> Result<Option<super::model::TenantConfig>, ConfigureError> {
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
    let config = repo::load(&tenant_pool.pool).await;
    tenant_pool.pool.close().await;

    Ok(config?)
}

async fn audit(
    control_pool: &PgPool,
    actor: &str,
    tenant_id: Option<uuid::Uuid>,
    input: &TenantConfigInput,
    outcome: &str,
) -> Result<(), sqlx::Error> {
    crate::platform_audit::record(
        control_pool,
        actor,
        "tenant_config.set",
        tenant_id,
        serde_json::json!({
            "retention_years": input.retention_years,
            "default_timezone": input.default_timezone,
            "default_locale": input.default_locale,
            "schedule_horizon_days": input.schedule_horizon_days,
            "quota_day_boundary_tz": input.quota_day_boundary_tz,
            "verification_mode": input.verification_mode,
            "staleness_max_age_seconds": input.staleness_max_age.microseconds / 1_000_000,
            "kill_switch_release_rate": input.kill_switch_release_rate,
            "outcome": outcome,
        }),
    )
    .await
}

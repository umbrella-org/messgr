use sqlx::PgPool;

use crate::profile::Profile;
use crate::tenant::pool::connect_tenant_pool;
use crate::tenant::repo as tenant_repo;

use super::model::{ProviderConfig, ProviderConfigInput};
use super::repo;

/// A configure/list run can fail on the control-database side, the
/// tenant-database side, or a domain-level rejection (an unknown
/// `tenant_slug`) — the last of these is carried as
/// `sqlx::Error::Configuration`, matching `tenant_config::ConfigureError`'s
/// shape.
#[derive(Debug)]
pub enum ConfigureError {
    Database(sqlx::Error),
}

impl std::fmt::Display for ConfigureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(err) => write!(f, "provider_config operation failed: {err}"),
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

/// Sets (creating or overwriting) one `(channel, priority)` row in a
/// tenant's provider list: resolves `tenant_slug`, opens its pool, loads the
/// existing row for that `(channel, priority)` (if any), and only writes
/// when the values actually differ — auditing exactly one
/// `provider_config.set` `platform_audit` row per call with outcome
/// `created`, `updated`, or `idempotent`.
///
/// An unknown `tenant_slug` audits a `rejected` row (`tenant_id: None`)
/// before returning the error — the T-005/F1 pattern (T-007 decision 8).
pub async fn set_provider_config(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    input: ProviderConfigInput,
    profile: Profile,
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

    let tenant_pool =
        connect_tenant_pool(base_db_url, &tenant.database_name, 5, profile).await?;

    let result =
        set_provider_config_inner(control_pool, &tenant_pool, tenant.id, actor, input)
            .await;

    tenant_pool.close().await;
    result
}

async fn set_provider_config_inner(
    control_pool: &PgPool,
    tenant_pool: &PgPool,
    tenant_id: uuid::Uuid,
    actor: &str,
    input: ProviderConfigInput,
) -> Result<ConfigureOutcome, ConfigureError> {
    let existing = repo::load_one(tenant_pool, &input.channel, input.priority).await?;

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

/// Lists a tenant's provider config for one channel, in failover order.
pub async fn list_provider_config(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    channel: &str,
    profile: Profile,
) -> Result<Vec<ProviderConfig>, ConfigureError> {
    let tenant = tenant_repo::find_by_slug(control_pool, tenant_slug)
        .await?
        .ok_or_else(|| {
            rejected(format!("no tenant registered with slug {tenant_slug:?}"))
        })?;

    let tenant_pool =
        connect_tenant_pool(base_db_url, &tenant.database_name, 5, profile).await?;
    let rows = repo::list(&tenant_pool, channel).await;
    tenant_pool.close().await;

    Ok(rows?)
}

async fn audit(
    control_pool: &PgPool,
    actor: &str,
    tenant_id: Option<uuid::Uuid>,
    input: &ProviderConfigInput,
    outcome: &str,
) -> Result<(), sqlx::Error> {
    crate::platform_audit::record(
        control_pool,
        actor,
        "provider_config.set",
        tenant_id,
        serde_json::json!({
            "channel": input.channel,
            "priority": input.priority,
            "provider": input.provider,
            "credential_path": input.credential_path,
            "rate_limit_per_sec": input.rate_limit_per_sec,
            "outcome": outcome,
        }),
    )
    .await
}

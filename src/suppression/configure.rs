use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::destination_hmac;
use crate::keystore::{KeyStore, KeyStoreError};
use crate::tenant::pool::connect_tenant_pool;
use crate::tenant::repo as tenant_repo;
use crate::tenant_pepper::{TenantPepperError, ensure_tenant_pepper};

use super::model::{Suppression, SuppressionInput};
use super::repo;

/// A configure/list run can fail on the control-database side, the
/// tenant-database side, the Vault side (unwrapping the tenant pepper), or a
/// domain-level rejection (an unknown `tenant_slug`, or `remove` against a
/// destination with no active entry) -- the last of these is carried as
/// `sqlx::Error::Configuration`, matching `provider_config::ConfigureError`'s
/// shape.
#[derive(Debug)]
pub enum ConfigureError {
    Database(sqlx::Error),
    Vault(KeyStoreError),
}

impl std::fmt::Display for ConfigureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(err) => write!(f, "suppression operation failed: {err}"),
            Self::Vault(err) => {
                write!(f, "suppression operation failed (vault): {err}")
            }
        }
    }
}

impl std::error::Error for ConfigureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(err) => Some(err),
            Self::Vault(err) => Some(err),
        }
    }
}

impl From<sqlx::Error> for ConfigureError {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err)
    }
}

impl From<KeyStoreError> for ConfigureError {
    fn from(err: KeyStoreError) -> Self {
        Self::Vault(err)
    }
}

impl From<TenantPepperError> for ConfigureError {
    fn from(err: TenantPepperError) -> Self {
        match err {
            TenantPepperError::Database(err) => Self::Database(err),
            TenantPepperError::Vault(err) => Self::Vault(err),
        }
    }
}

fn rejected(message: String) -> ConfigureError {
    sqlx::Error::Configuration(message.into()).into()
}

fn truncate_to_micros(dt: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp_micros(dt.timestamp_micros())
        .expect("timestamp_micros() output is always a valid instant")
}

#[derive(Debug)]
pub struct ConfigureOutcome {
    /// "created" | "updated" | "idempotent" -- never "rejected", which is
    /// returned as an `Err` instead.
    pub outcome: &'static str,
}

/// Adds a new suppression entry, or updates an existing one's
/// `reason`/`review_at`: resolves `tenant_slug`, opens its pool, derives
/// `destination_hmac` from the raw `destination` via the tenant's pepper
/// (never stored or logged in plaintext), loads any existing row for that
/// hash, and only writes when the values actually differ -- auditing
/// exactly one `suppression.add` `platform_audit` row per call with outcome
/// `created`, `updated`, or `idempotent`.
///
/// An unknown `tenant_slug` audits a `rejected` row (`tenant_id: None`)
/// before returning the error -- the T-005/F1 pattern.
#[allow(clippy::too_many_arguments)]
pub async fn add_suppression(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    keystore: &dyn KeyStore,
    destination: &str,
    reason: &str,
    review_at: DateTime<Utc>,
    actor: &str,
) -> Result<ConfigureOutcome, ConfigureError> {
    // Postgres `timestamptz` stores microsecond precision; `DateTime<Utc>`
    // carries nanoseconds. Truncate here, before the first comparison or
    // write, so `matches` (below) never compares a full-precision caller
    // value against a microsecond-truncated one read back from storage.
    let review_at = truncate_to_micros(review_at);

    let tenant = match tenant_repo::find_by_slug(control_pool, tenant_slug).await? {
        Some(tenant) => tenant,
        None => {
            audit_add(
                control_pool,
                actor,
                None,
                None,
                reason,
                review_at,
                "rejected",
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

    let pepper = ensure_tenant_pepper(control_pool, keystore, &tenant).await?;
    let destination_hmac = destination_hmac::compute(&pepper, destination);

    let input = SuppressionInput {
        destination_hmac,
        reason: reason.to_string(),
        review_at,
    };

    let result =
        add_suppression_inner(control_pool, &tenant_pool.pool, tenant.id, actor, input)
            .await;

    tenant_pool.pool.close().await;
    result
}

async fn add_suppression_inner(
    control_pool: &PgPool,
    tenant_pool: &PgPool,
    tenant_id: uuid::Uuid,
    actor: &str,
    input: SuppressionInput,
) -> Result<ConfigureOutcome, ConfigureError> {
    let existing = repo::load_one(tenant_pool, &input.destination_hmac).await?;

    let outcome = match &existing {
        None => "created",
        Some(existing) if input.matches(existing) => "idempotent",
        Some(_) => "updated",
    };

    if outcome != "idempotent" {
        repo::upsert(tenant_pool, &input, Utc::now()).await?;
    }

    audit_add(
        control_pool,
        actor,
        Some(tenant_id),
        Some(&input.destination_hmac),
        &input.reason,
        input.review_at,
        outcome,
    )
    .await?;

    Ok(ConfigureOutcome { outcome })
}

/// Retires an active entry early by moving `review_at` to now -- the exact
/// same mechanism as letting one expire naturally (T-038 decision 2). A
/// destination with no currently-active entry (already expired, or never
/// suppressed) is a rejection, audited the same way `set_provider_config`'s
/// unknown-tenant-slug case is (T-005/F1: audit the rejection, then error --
/// decision 8).
pub async fn remove_suppression(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    keystore: &dyn KeyStore,
    destination: &str,
    actor: &str,
) -> Result<(), ConfigureError> {
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

    let pepper = ensure_tenant_pepper(control_pool, keystore, &tenant).await?;
    let destination_hmac = destination_hmac::compute(&pepper, destination);

    let rows_affected =
        repo::retire_now(&tenant_pool.pool, &destination_hmac, Utc::now()).await;
    tenant_pool.pool.close().await;
    let rows_affected = rows_affected?;

    if rows_affected == 0 {
        audit_remove(
            control_pool,
            actor,
            Some(tenant.id),
            &destination_hmac,
            "rejected",
        )
        .await?;
        return Err(rejected(
            "no active suppression entry for this destination".to_string(),
        ));
    }

    audit_remove(
        control_pool,
        actor,
        Some(tenant.id),
        &destination_hmac,
        "retired",
    )
    .await?;
    Ok(())
}

/// Lists every suppression entry for a tenant -- no pepper needed, nothing
/// to reverse: `destination_hmac` is one-way.
pub async fn list_suppression(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
) -> Result<Vec<Suppression>, ConfigureError> {
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

/// Never audits the raw `destination` -- only the derived
/// `destination_hmac` (hex-encoded), matching why the table itself is keyed
/// this way (T-038 decision 6).
async fn audit_add(
    control_pool: &PgPool,
    actor: &str,
    tenant_id: Option<uuid::Uuid>,
    destination_hmac: Option<&[u8]>,
    reason: &str,
    review_at: DateTime<Utc>,
    outcome: &str,
) -> Result<(), sqlx::Error> {
    crate::platform_audit::record(
        control_pool,
        actor,
        "suppression.add",
        tenant_id,
        serde_json::json!({
            "destination_hmac": destination_hmac.map(hex_encode),
            "reason": reason,
            "review_at": review_at,
            "outcome": outcome,
        }),
    )
    .await
}

async fn audit_remove(
    control_pool: &PgPool,
    actor: &str,
    tenant_id: Option<uuid::Uuid>,
    destination_hmac: &[u8],
    outcome: &str,
) -> Result<(), sqlx::Error> {
    crate::platform_audit::record(
        control_pool,
        actor,
        "suppression.remove",
        tenant_id,
        serde_json::json!({
            "destination_hmac": hex_encode(destination_hmac),
            "outcome": outcome,
        }),
    )
    .await
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

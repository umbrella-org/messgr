//! Deletes `idempotency` rows past their retention window (DESIGN.md §4.3:
//! "retained 30 days, swept nightly", T-029). `as_of` is always an explicit
//! parameter (mirrors `partition_lifecycle`'s own decision 4) so tests can
//! synthesize expired/not-yet-expired rows deterministically.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::tenant::pool::connect_tenant_pool;
use crate::tenant::repo as tenant_repo;

#[derive(Debug)]
pub enum SweepError {
    Database(sqlx::Error),
    UnknownTenant(String),
}

impl std::fmt::Display for SweepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(err) => write!(f, "idempotency sweep failed: {err}"),
            Self::UnknownTenant(slug) => {
                write!(f, "no tenant registered with slug {slug:?}")
            }
        }
    }
}

impl std::error::Error for SweepError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(err) => Some(err),
            Self::UnknownTenant(_) => None,
        }
    }
}

/// Resolves `tenant_slug`, opens its pool, and deletes every `idempotency`
/// row whose `expires_at` is at or before `as_of`. Returns the number of
/// rows removed.
pub async fn run_for_tenant(
    control_pool: &PgPool,
    control_database_url: &str,
    tenant_slug: &str,
    as_of: DateTime<Utc>,
    max_connections: u32,
) -> Result<u64, SweepError> {
    let tenant = tenant_repo::find_by_slug(control_pool, tenant_slug)
        .await
        .map_err(SweepError::Database)?
        .ok_or_else(|| SweepError::UnknownTenant(tenant_slug.to_string()))?;

    let tenant_pool = connect_tenant_pool(
        control_pool,
        control_database_url,
        tenant.id,
        &tenant.database_name,
        max_connections,
    )
    .await
    .map_err(SweepError::Database)?;

    let result = sweep(&tenant_pool.pool, as_of).await;

    tenant_pool.pool.close().await;
    result
}

async fn sweep(tenant_pool: &PgPool, as_of: DateTime<Utc>) -> Result<u64, SweepError> {
    let result = sqlx::query("DELETE FROM idempotency WHERE expires_at <= $1")
        .bind(as_of)
        .execute(tenant_pool)
        .await
        .map_err(SweepError::Database)?;

    Ok(result.rows_affected())
}

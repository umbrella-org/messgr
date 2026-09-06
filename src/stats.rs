//! Per-tenant message-volume counts for `messgr-control stats` (DESIGN.md
//! §11.4: counts/metadata for platform tooling, never payload content;
//! T-028). Reads `comms_request.final_status` directly -- no join to
//! `outbox`/`comms_event` needed, since the terminal outcome already lives
//! on the ledger row (§4.1).

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::PgPool;

use crate::tenant::pool::connect_tenant_pool;
use crate::tenant::repo as tenant_repo;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChannelStatusCount {
    pub channel: String,
    pub status: String,
    pub count: i64,
}

/// A stats run can fail on the control-database side, the tenant-database
/// side, or a domain-level rejection (an unknown `tenant_slug`) — the last
/// of these is carried as `sqlx::Error::Configuration`, matching
/// `provider_config`/`tenant_config`'s `ConfigureError` shape (T-025 item 7:
/// this used to be a bare `sqlx::Error`, unlike every sibling
/// resolve/connect/act function).
#[derive(Debug)]
pub enum StatsError {
    Database(sqlx::Error),
}

impl std::fmt::Display for StatsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(err) => write!(f, "stats operation failed: {err}"),
        }
    }
}

impl std::error::Error for StatsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(err) => Some(err),
        }
    }
}

impl From<sqlx::Error> for StatsError {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err)
    }
}

/// Resolves `tenant_slug`, opens its pool, runs the count query, closes the
/// pool. Mirrors `partition_lifecycle::lifecycle::run_for_tenant`'s
/// resolve/connect/close shape so `src/bin/control.rs` stays as thin as
/// every other command.
pub async fn tenant_message_stats(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    since: Option<NaiveDate>,
) -> Result<Vec<ChannelStatusCount>, StatsError> {
    let tenant = tenant_repo::find_by_slug(control_pool, tenant_slug)
        .await?
        .ok_or_else(|| {
            sqlx::Error::Configuration(
                format!("no tenant registered with slug {tenant_slug:?}").into(),
            )
        })?;

    let tenant_pool = connect_tenant_pool(
        control_pool,
        base_db_url,
        tenant.id,
        &tenant.database_name,
        5,
    )
    .await?;

    let since_ts: Option<DateTime<Utc>> = since
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc());

    let result = sqlx::query_as::<_, ChannelStatusCount>(
        r#"
        SELECT
            channel,
            coalesce(final_status, 'pending') AS status,
            count(*) AS count
        FROM comms_request
        WHERE channel IN ('sms', 'email')
          AND ($1::timestamptz IS NULL OR created_at >= $1)
        GROUP BY channel, status
        ORDER BY channel, status
        "#,
    )
    .bind(since_ts)
    .fetch_all(&tenant_pool.pool)
    .await;

    tenant_pool.pool.close().await;
    result.map_err(StatsError::from)
}

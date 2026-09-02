//! Per-tenant message-volume counts for `messgr-control stats` (DESIGN.md
//! §11.4: counts/metadata for platform tooling, never payload content;
//! T-028). Reads `comms_request.final_status` directly -- no join to
//! `outbox`/`comms_event` needed, since the terminal outcome already lives
//! on the ledger row (§4.1).

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::PgPool;

use crate::profile::Profile;
use crate::tenant::pool::connect_tenant_pool;
use crate::tenant::repo as tenant_repo;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChannelStatusCount {
    pub channel: String,
    pub status: String,
    pub count: i64,
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
    profile: Profile,
) -> Result<Vec<ChannelStatusCount>, sqlx::Error> {
    let tenant = tenant_repo::find_by_slug(control_pool, tenant_slug)
        .await?
        .ok_or_else(|| {
            sqlx::Error::Configuration(
                format!("no tenant registered with slug {tenant_slug:?}").into(),
            )
        })?;

    let tenant_pool =
        connect_tenant_pool(base_db_url, &tenant.database_name, 5, profile).await?;

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
    .fetch_all(&tenant_pool)
    .await;

    tenant_pool.close().await;
    result
}

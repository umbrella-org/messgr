use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::model::{
    ProducerQuota, ProducerQuotaInput, ProducerQuotaOverride,
    ProducerQuotaOverrideInput, UsageRow,
};

pub async fn load_one(
    pool: &PgPool,
    producer_id: Uuid,
    channel: &str,
    class: &str,
) -> Result<Option<ProducerQuota>, sqlx::Error> {
    sqlx::query_as::<_, ProducerQuota>(
        "SELECT producer_id, channel, class, per_minute, per_day, enforcement \
         FROM producer_quota WHERE producer_id = $1 AND channel = $2 AND class = $3",
    )
    .bind(producer_id)
    .bind(channel)
    .bind(class)
    .fetch_optional(pool)
    .await
}

pub async fn list(pool: &PgPool) -> Result<Vec<ProducerQuota>, sqlx::Error> {
    sqlx::query_as::<_, ProducerQuota>(
        "SELECT producer_id, channel, class, per_minute, per_day, enforcement \
         FROM producer_quota ORDER BY producer_id, channel, class",
    )
    .fetch_all(pool)
    .await
}

pub async fn upsert(
    pool: &PgPool,
    input: &ProducerQuotaInput,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO producer_quota (producer_id, channel, class, per_minute, per_day, enforcement)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (producer_id, channel, class) DO UPDATE SET
            per_minute = EXCLUDED.per_minute,
            per_day = EXCLUDED.per_day,
            enforcement = EXCLUDED.enforcement
        "#,
    )
    .bind(input.producer_id)
    .bind(&input.channel)
    .bind(&input.class)
    .bind(input.per_minute)
    .bind(input.per_day)
    .bind(&input.enforcement)
    .execute(pool)
    .await
    .map(|_| ())
}

pub async fn insert_override(
    pool: &PgPool,
    id: Uuid,
    input: &ProducerQuotaOverrideInput,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO producer_quota_override
            (id, producer_id, channel, class, per_day, valid_from, valid_to, approved_by, reason)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        "#,
    )
    .bind(id)
    .bind(input.producer_id)
    .bind(&input.channel)
    .bind(&input.class)
    .bind(input.per_day)
    .bind(input.valid_from)
    .bind(input.valid_to)
    .bind(&input.approved_by)
    .bind(&input.reason)
    .execute(pool)
    .await
    .map(|_| ())
}

pub async fn list_overrides(
    pool: &PgPool,
) -> Result<Vec<ProducerQuotaOverride>, sqlx::Error> {
    sqlx::query_as::<_, ProducerQuotaOverride>(
        "SELECT id, producer_id, channel, class, per_day, valid_from, valid_to, approved_by, reason \
         FROM producer_quota_override ORDER BY valid_from DESC",
    )
    .fetch_all(pool)
    .await
}

/// Used by the tracker's config refresh — every override currently active
/// at `now`.
pub async fn load_active_overrides(
    pool: &PgPool,
    now: DateTime<Utc>,
) -> Result<Vec<ProducerQuotaOverride>, sqlx::Error> {
    sqlx::query_as::<_, ProducerQuotaOverride>(
        "SELECT id, producer_id, channel, class, per_day, valid_from, valid_to, approved_by, reason \
         FROM producer_quota_override WHERE valid_from <= $1 AND valid_to > $1",
    )
    .bind(now)
    .fetch_all(pool)
    .await
}

/// Used by `QuotaTracker::rebuild_from_db` at dispatcher startup.
pub async fn load_current_usage(
    pool: &PgPool,
    minute_window_start: DateTime<Utc>,
    day_window_start: DateTime<Utc>,
) -> Result<Vec<UsageRow>, sqlx::Error> {
    sqlx::query_as::<_, UsageRow>(
        "SELECT producer_id, channel, class, granularity, window_start, sent, blocked \
         FROM producer_usage \
         WHERE (granularity = 'minute' AND window_start = $1) \
            OR (granularity = 'day' AND window_start = $2)",
    )
    .bind(minute_window_start)
    .bind(day_window_start)
    .fetch_all(pool)
    .await
}

/// One `INSERT ... ON CONFLICT DO UPDATE` per row, in one transaction — each
/// row's `sent`/`blocked` is the in-process counter's own current total for
/// that window, an overwrite snapshot, not an increment (the in-memory value
/// is already authoritative).
pub async fn flush_usage(pool: &PgPool, rows: &[UsageRow]) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    for row in rows {
        sqlx::query(
            r#"
            INSERT INTO producer_usage
                (producer_id, channel, class, granularity, window_start, sent, blocked)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            ON CONFLICT (producer_id, channel, class, granularity, window_start) DO UPDATE SET
                sent = EXCLUDED.sent,
                blocked = EXCLUDED.blocked
            "#,
        )
        .bind(row.producer_id)
        .bind(&row.channel)
        .bind(&row.class)
        .bind(&row.granularity)
        .bind(row.window_start)
        .bind(row.sent)
        .bind(row.blocked)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

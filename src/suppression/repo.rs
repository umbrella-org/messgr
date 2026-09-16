use chrono::{DateTime, Utc};
use sqlx::PgPool;

use super::model::{Suppression, SuppressionInput};

pub async fn list(pool: &PgPool) -> Result<Vec<Suppression>, sqlx::Error> {
    sqlx::query_as::<_, Suppression>(
        "SELECT destination_hmac, reason, added_at, review_at FROM suppression ORDER BY added_at",
    )
    .fetch_all(pool)
    .await
}

pub async fn load_one(
    pool: &PgPool,
    destination_hmac: &[u8],
) -> Result<Option<Suppression>, sqlx::Error> {
    sqlx::query_as::<_, Suppression>(
        "SELECT destination_hmac, reason, added_at, review_at FROM suppression \
         WHERE destination_hmac = $1",
    )
    .bind(destination_hmac)
    .fetch_optional(pool)
    .await
}

pub async fn upsert(
    pool: &PgPool,
    input: &SuppressionInput,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO suppression (destination_hmac, reason, added_at, review_at)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (destination_hmac) DO UPDATE SET
            reason = EXCLUDED.reason,
            review_at = EXCLUDED.review_at
        "#,
    )
    .bind(&input.destination_hmac)
    .bind(&input.reason)
    .bind(now)
    .bind(input.review_at)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Retires an entry early by moving `review_at` to `now` -- a no-op (0 rows
/// affected) against an entry that's already expired or was never
/// suppressed, so the caller (`configure::remove_suppression`) can use the
/// row count to decide rejected vs. retired (T-038 decision 8).
pub async fn retire_now(
    pool: &PgPool,
    destination_hmac: &[u8],
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE suppression SET review_at = $2 WHERE destination_hmac = $1 AND review_at > $2",
    )
    .bind(destination_hmac)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

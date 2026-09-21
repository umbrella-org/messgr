use super::filter::OutboxQueueFilter;
use super::model::OutboxQueueRow;

/// Queries `outbox` directly (not `comms_request`, which has no
/// `next_attempt_at` column) — served by the existing `(producer_id,
/// next_attempt_at)` / `(campaign_id, next_attempt_at)` indexes (T-049
/// decision 6).
pub async fn list(
    pool: &sqlx::PgPool,
    filter: &OutboxQueueFilter,
    limit: i64,
) -> Result<Vec<OutboxQueueRow>, sqlx::Error> {
    sqlx::query_as::<_, OutboxQueueRow>(
        r#"
        SELECT comms_request_id, created_at, channel, class, producer_id,
               campaign_id, next_attempt_at, expires_at
        FROM outbox
        WHERE cancelled_at IS NULL
          AND ($1::uuid IS NULL OR producer_id = $1)
          AND ($2::text IS NULL OR campaign_id = $2)
          AND ($3::timestamptz IS NULL OR next_attempt_at <= $3)
          AND ($4::timestamptz IS NULL OR next_attempt_at >= $4)
        ORDER BY next_attempt_at
        LIMIT $5
        "#,
    )
    .bind(filter.producer_id)
    .bind(&filter.campaign_id)
    .bind(filter.due_before)
    .bind(filter.due_after)
    .bind(limit)
    .fetch_all(pool)
    .await
}

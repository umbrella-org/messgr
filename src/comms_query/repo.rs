use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::filter::CommsFilter;
use super::model::{
    CampaignReachCount, CommsEventRow, CommsRequestDetail, CommsRequestSummary,
};

const SUMMARY_COLUMNS: &str = "id, created_at, customer_id, channel, class, template_id, \
     template_version, campaign_id, producer_id, scheduled_for, expires_at, final_status, \
     finalized_at";

/// One fixed query; each filter field applied as `($n::type IS NULL OR
/// column = $n)` — every predicate here is optional-equality or a range,
/// which this pattern covers without `sqlx::QueryBuilder` (T-048 Task 5).
pub async fn list(
    pool: &PgPool,
    filter: &CommsFilter,
    limit: i64,
) -> Result<Vec<CommsRequestSummary>, sqlx::Error> {
    sqlx::query_as::<_, CommsRequestSummary>(&format!(
        r#"
        SELECT {SUMMARY_COLUMNS}
        FROM comms_request
        WHERE ($1::uuid IS NULL OR customer_id = $1)
          AND ($2::text IS NULL OR channel = $2)
          AND ($3::text IS NULL OR class = $3)
          AND ($4::text IS NULL OR campaign_id = $4)
          AND ($5::uuid IS NULL OR producer_id = $5)
          AND ($6::timestamptz IS NULL OR created_at >= $6)
          AND ($7::timestamptz IS NULL OR created_at <= $7)
          AND ($8::text IS NULL OR final_status = $8)
          AND ($9::bool IS NULL OR (scheduled_for IS NOT NULL AND final_status IS NULL) = $9)
        ORDER BY created_at DESC
        LIMIT $10
        "#
    ))
    .bind(filter.customer_id)
    .bind(&filter.channel)
    .bind(&filter.class)
    .bind(&filter.campaign_id)
    .bind(filter.producer_id)
    .bind(filter.from)
    .bind(filter.to)
    .bind(&filter.status)
    .bind(filter.scheduled)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Decision 6's compound key — `comms_request`'s primary key is
/// `(created_at, id)`. Returns the row with its ciphertext columns still
/// wrapped; decrypting them is the caller's job (decision 14).
pub async fn detail(
    pool: &PgPool,
    created_at: DateTime<Utc>,
    id: Uuid,
) -> Result<Option<CommsRequestDetail>, sqlx::Error> {
    sqlx::query_as::<_, CommsRequestDetail>(
        r#"
        SELECT id, created_at, customer_id, channel, class, template_id, template_version,
               campaign_id, destination_ciphertext, payload_ciphertext, producer_id,
               scheduled_for, expires_at, final_status, finalized_at
        FROM comms_request
        WHERE created_at = $1 AND id = $2
        "#,
    )
    .bind(created_at)
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// Uses Task 2's new `(comms_request_id, occurred_at)` index.
pub async fn events(
    pool: &PgPool,
    comms_request_id: Uuid,
) -> Result<Vec<CommsEventRow>, sqlx::Error> {
    sqlx::query_as::<_, CommsEventRow>(
        r#"
        SELECT comms_request_id, customer_id, occurred_at, event_type, provider_ref, provider_status
        FROM comms_event
        WHERE comms_request_id = $1
        ORDER BY occurred_at
        "#,
    )
    .bind(comms_request_id)
    .fetch_all(pool)
    .await
}

/// `canonical_customer_id`'s alias set (decision 15) first, then every
/// `comms_request` row under any of those ids — so a row written under a
/// since-superseded id still shows up.
pub async fn timeline(
    pool: &PgPool,
    canonical_customer_id: Uuid,
    limit: i64,
) -> Result<Vec<CommsRequestSummary>, sqlx::Error> {
    let ids = crate::customer::repo::alias_set(pool, canonical_customer_id).await?;

    sqlx::query_as::<_, CommsRequestSummary>(&format!(
        r#"
        SELECT {SUMMARY_COLUMNS}
        FROM comms_request
        WHERE customer_id = ANY($1)
        ORDER BY created_at DESC
        LIMIT $2
        "#
    ))
    .bind(&ids)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// §11.2: "aggregating on `final_status`"; `NULL` means still in flight.
/// Uses the `(campaign_id, created_at)` index already on `comms_request`
/// since migration 0004.
pub async fn campaign_reach(
    pool: &PgPool,
    campaign_id: &str,
) -> Result<Vec<CampaignReachCount>, sqlx::Error> {
    let rows: Vec<(Option<String>, i64)> = sqlx::query_as(
        "SELECT final_status, COUNT(*) FROM comms_request WHERE campaign_id = $1 \
         GROUP BY final_status",
    )
    .bind(campaign_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(final_status, count)| CampaignReachCount {
            final_status,
            count,
        })
        .collect())
}

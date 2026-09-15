//! Database queries for orphan-event reconciliation (T-030): matching
//! pending `orphan_event` rows against `comms_event.provider_ref`,
//! promoting a match into a real `comms_event` row, and ageing out rows past
//! the reconcile-attempts cap.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, sqlx::FromRow)]
pub struct PendingOrphan {
    pub id: Uuid,
    pub provider_ref: String,
    pub event_type: String,
    pub provider_status: Option<String>,
    pub provider_payload_raw: Option<serde_json::Value>,
    pub occurred_at: DateTime<Utc>,
    pub reconcile_attempts: i16,
}

pub struct Match {
    pub comms_request_id: Uuid,
    pub comms_request_created_at: DateTime<Utc>,
    pub customer_id: Uuid,
    pub current_final_status: Option<String>,
}

pub async fn list_pending(pool: &PgPool) -> Result<Vec<PendingOrphan>, sqlx::Error> {
    sqlx::query_as::<_, PendingOrphan>(
        r#"
        SELECT id, provider_ref, event_type, provider_status, provider_payload_raw,
               occurred_at, reconcile_attempts
        FROM orphan_event
        "#,
    )
    .fetch_all(pool)
    .await
}

/// `comms_request.id` is not itself unique per §4.1's partitioned
/// `PRIMARY KEY (created_at, id)`, but is generated as a fresh UUID per
/// request, so joining on `id` alone (without `created_at`) is safe in
/// practice and matches this ticket's own Description.
///
/// Excludes `ce.provider_ref = ''` — that is not a real provider reference,
/// it is the default every dispatch-internal `comms_event` row carries when
/// the provider gave none (migration 0004's own comment on the column).
/// Without this guard, an `orphan_event` row that itself ends up with an
/// empty `provider_ref` (a malformed receipt, say) would match an
/// arbitrary, unrelated `comms_request` here instead of finding nothing —
/// silently promoting a stranger's third-party payload under a stranger's
/// DEK and mutating that stranger's `final_status` (T-030 review finding
/// F1).
///
/// Orders by `occurred_at DESC` so the most recently occurred row wins when
/// several `comms_event` rows legitimately share a `provider_ref` (e.g.
/// `sent` and `delivered` on the same request) — otherwise `LIMIT 1` picks
/// whichever Postgres happens to return first (T-030 review finding F5).
/// `comms_request_id DESC` breaks a further tie on `occurred_at` itself
/// (two rows sharing both a `provider_ref` and a timestamp), so the choice
/// stays fully deterministic rather than only "usually" (T-034 review).
///
/// `comms_event` is partitioned by `occurred_at` (§4.4) and `provider_ref`
/// is not the partition key, so this `ORDER BY` can no longer let the
/// planner stop at the first partition with a match the way a bare
/// `LIMIT 1` could — it must consider every partition the per-partition
/// `provider_ref` index (migration 0011) can return a row from before
/// picking the most recent. Each such probe is a cheap index lookup, so
/// this trades a little more planning work for the correctness this
/// function now guarantees (T-034 review).
pub async fn find_match(
    pool: &PgPool,
    provider_ref: &str,
) -> Result<Option<Match>, sqlx::Error> {
    let row = sqlx::query_as::<_, (Uuid, DateTime<Utc>, Uuid, Option<String>)>(
        r#"
        SELECT cr.id, cr.created_at, cr.customer_id, cr.final_status
        FROM comms_event ce
        JOIN comms_request cr ON cr.id = ce.comms_request_id
        WHERE ce.provider_ref = $1 AND ce.provider_ref <> ''
        ORDER BY ce.occurred_at DESC, ce.comms_request_id DESC
        LIMIT 1
        "#,
    )
    .bind(provider_ref)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(
        |(
            comms_request_id,
            comms_request_created_at,
            customer_id,
            current_final_status,
        )| Match {
            comms_request_id,
            comms_request_created_at,
            customer_id,
            current_final_status,
        },
    ))
}

/// One transaction: inserts the promoted `comms_event` row (ciphertext
/// already computed by the caller — this module never touches the
/// DEK/keystore), conditionally advances `comms_request.final_status` per
/// the caller's `advance` verdict, and deletes the now-redundant
/// `orphan_event` row.
pub async fn promote(
    pool: &PgPool,
    orphan: &PendingOrphan,
    m: &Match,
    ciphertext: Option<Vec<u8>>,
    advance: bool,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;

    sqlx::query(
        r#"
        INSERT INTO comms_event (
            comms_request_id, customer_id, occurred_at, event_type, provider_ref,
            provider_status, provider_payload_ciphertext
        ) VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (occurred_at, comms_request_id, event_type, provider_ref) DO NOTHING
        "#,
    )
    .bind(m.comms_request_id)
    .bind(m.customer_id)
    .bind(orphan.occurred_at)
    .bind(&orphan.event_type)
    .bind(&orphan.provider_ref)
    .bind(orphan.provider_status.as_deref())
    .bind(ciphertext)
    .execute(&mut *tx)
    .await?;

    if advance {
        sqlx::query(
            "UPDATE comms_request SET final_status = $1, finalized_at = $2 \
             WHERE created_at = $3 AND id = $4",
        )
        .bind(&orphan.event_type)
        .bind(Utc::now())
        .bind(m.comms_request_created_at)
        .bind(m.comms_request_id)
        .execute(&mut *tx)
        .await?;
    }

    sqlx::query("DELETE FROM orphan_event WHERE id = $1")
        .bind(orphan.id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await
}

/// Increments `reconcile_attempts`, then deletes the row if it has now
/// reached `cap` — done as two statements against the same value rather
/// than a single conditional one so the row is never left silently sitting
/// at `reconcile_attempts >= cap` without being removed on the same call
/// that pushed it there.
pub async fn record_miss(
    pool: &PgPool,
    orphan_id: Uuid,
    cap: i16,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE orphan_event SET reconcile_attempts = reconcile_attempts + 1 WHERE id = $1",
    )
    .bind(orphan_id)
    .execute(pool)
    .await?;

    sqlx::query("DELETE FROM orphan_event WHERE id = $1 AND reconcile_attempts >= $2")
        .bind(orphan_id)
        .bind(cap)
        .execute(pool)
        .await?;

    Ok(())
}

//! Database queries for `webhook_receipt_staging` (T-047): the DMZ-side
//! insert `messgr-webhook` itself performs, plus the reads/writes the
//! internal `webhook-promote` step needs to drain it.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::orphan_reconcile::repo::{self as orphan_repo, Match};

#[derive(Debug, sqlx::FromRow)]
pub struct PendingStagingReceipt {
    pub id: Uuid,
    pub provider: String,
    pub provider_ref: String,
    pub occurred_at: DateTime<Utc>,
    pub event_type: String,
    pub provider_status: Option<String>,
    pub provider_payload_raw: serde_json::Value,
}

/// `messgr-webhook`'s only SQL statement, ever (T-047 decision 3). A
/// provider's own retries before the next promoter run are free, same
/// reasoning as `comms_event`'s own constraint.
#[allow(clippy::too_many_arguments)]
pub async fn insert_staging(
    pool: &PgPool,
    id: Uuid,
    received_at: DateTime<Utc>,
    provider: &str,
    provider_ref: &str,
    occurred_at: DateTime<Utc>,
    event_type: &str,
    provider_status: Option<&str>,
    provider_payload_raw: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO webhook_receipt_staging (
            id, received_at, provider, provider_ref, occurred_at, event_type,
            provider_status, provider_payload_raw
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        ON CONFLICT (provider, provider_ref, event_type, occurred_at) DO NOTHING
        "#,
    )
    .bind(id)
    .bind(received_at)
    .bind(provider)
    .bind(provider_ref)
    .bind(occurred_at)
    .bind(event_type)
    .bind(provider_status)
    .bind(provider_payload_raw)
    .execute(pool)
    .await
    .map(|_| ())
}

pub async fn list_pending(
    pool: &PgPool,
) -> Result<Vec<PendingStagingReceipt>, sqlx::Error> {
    sqlx::query_as::<_, PendingStagingReceipt>(
        r#"
        SELECT id, provider, provider_ref, occurred_at, event_type, provider_status, provider_payload_raw
        FROM webhook_receipt_staging
        "#,
    )
    .fetch_all(pool)
    .await
}

/// One transaction: inserts the promoted `comms_event` row (via
/// `orphan_reconcile::repo::insert_comms_event`, the same statement
/// `orphan_reconcile::repo::promote` uses) and deletes the now-redundant
/// `webhook_receipt_staging` row.
pub async fn promote(
    pool: &PgPool,
    receipt: &PendingStagingReceipt,
    m: &Match,
    ciphertext: Option<Vec<u8>>,
    advance: bool,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;

    orphan_repo::insert_comms_event(
        &mut tx,
        m.comms_request_id,
        m.comms_request_created_at,
        m.customer_id,
        receipt.occurred_at,
        &receipt.event_type,
        &receipt.provider_ref,
        receipt.provider_status.as_deref(),
        ciphertext,
        advance,
    )
    .await?;

    sqlx::query("DELETE FROM webhook_receipt_staging WHERE id = $1")
        .bind(receipt.id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await
}

/// No match yet: hands the row to `orphan_event`, unencrypted (the
/// documented exception, `03-data-model.md:215`), for T-030's existing
/// reconciler to pick up on its own schedule, then deletes the staging row.
pub async fn to_orphan(
    pool: &PgPool,
    receipt: &PendingStagingReceipt,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;

    sqlx::query(
        r#"
        INSERT INTO orphan_event (
            id, received_at, provider, provider_ref, occurred_at, event_type,
            provider_status, provider_payload_raw, reconcile_attempts
        ) VALUES ($1, now(), $2, $3, $4, $5, $6, $7, 0)
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(&receipt.provider)
    .bind(&receipt.provider_ref)
    .bind(receipt.occurred_at)
    .bind(&receipt.event_type)
    .bind(receipt.provider_status.as_deref())
    .bind(&receipt.provider_payload_raw)
    .execute(&mut *tx)
    .await?;

    sqlx::query("DELETE FROM webhook_receipt_staging WHERE id = $1")
        .bind(receipt.id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await
}

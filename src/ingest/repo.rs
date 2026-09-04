//! Idempotency claim + single-transaction ledger/outbox write (DESIGN.md
//! §4.1–§4.3, T-011 decision on the claim-then-insert pattern).

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

pub const IDEMPOTENCY_TTL_DAYS: i64 = 30;

pub enum InsertOutcome {
    Created,
    /// Another request already claimed this idempotency key — no new row
    /// was written; the caller returns this id with `200`, not `201`.
    Replayed {
        comms_request_id: Uuid,
    },
}

/// Fast pre-check for the common "this is a genuine retry" case — skips
/// template lookup, DEK fetch, and encryption entirely when the key is
/// already known. Not itself the source of correctness: `insert_transactional`'s
/// own `ON CONFLICT DO NOTHING` is what makes concurrent identical retries
/// safe; this is purely an optimization.
pub async fn find_idempotent_reply(
    pool: &PgPool,
    producer_id: Uuid,
    key: &str,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT comms_request_id FROM idempotency WHERE producer_id = $1 AND key = $2",
    )
    .bind(producer_id)
    .bind(key)
    .fetch_optional(pool)
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn insert_transactional(
    pool: &PgPool,
    idempotency_key: &str,
    comms_request_id: Uuid,
    tenant_id: Uuid,
    customer_id: Uuid,
    channel: &str,
    class: &str,
    priority: i16,
    template_id: &str,
    template_version: i32,
    campaign_id: Option<&str>,
    destination_hmac: &[u8],
    destination_ciphertext: &[u8],
    payload_ciphertext: &[u8],
    producer_id: Uuid,
    address_id: Uuid,
) -> Result<InsertOutcome, sqlx::Error> {
    let now: DateTime<Utc> = Utc::now();
    let idempotency_expires_at = now + Duration::days(IDEMPOTENCY_TTL_DAYS);

    let mut tx = pool.begin().await?;

    let claim = sqlx::query(
        "INSERT INTO idempotency (producer_id, key, comms_request_id, expires_at) \
         VALUES ($1, $2, $3, $4) ON CONFLICT (producer_id, key) DO NOTHING",
    )
    .bind(producer_id)
    .bind(idempotency_key)
    .bind(comms_request_id)
    .bind(idempotency_expires_at)
    .execute(&mut *tx)
    .await?;

    if claim.rows_affected() == 0 {
        tx.rollback().await?;
        let existing = find_idempotent_reply(pool, producer_id, idempotency_key).await?.expect(
            "a row must exist immediately after losing the idempotency claim race",
        );
        return Ok(InsertOutcome::Replayed {
            comms_request_id: existing,
        });
    }

    sqlx::query(
        r#"
        INSERT INTO comms_request (
            tenant_id, id, created_at, customer_id, channel, class, template_id,
            template_version, campaign_id, destination_hmac, destination_ciphertext,
            payload_ciphertext, producer_id, scheduled_for, expires_at,
            final_status, finalized_at
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, NULL, NULL, NULL, NULL
        )
        "#,
    )
    .bind(tenant_id)
    .bind(comms_request_id)
    .bind(now)
    .bind(customer_id)
    .bind(channel)
    .bind(class)
    .bind(template_id)
    .bind(template_version)
    .bind(campaign_id)
    .bind(destination_hmac)
    .bind(destination_ciphertext)
    .bind(payload_ciphertext)
    .bind(producer_id)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO outbox (
            comms_request_id, created_at, channel, class, priority, customer_id,
            address_id, producer_id, campaign_id, next_attempt_at, expires_at,
            cancelled_at, attempts, leased_until
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, NULL, NULL, 0, NULL
        )
        "#,
    )
    .bind(comms_request_id)
    .bind(now)
    .bind(channel)
    .bind(class)
    .bind(priority)
    .bind(customer_id)
    .bind(address_id)
    .bind(producer_id)
    .bind(campaign_id)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(InsertOutcome::Created)
}

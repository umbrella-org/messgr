//! One best-effort transaction: `comms_request` insert with `final_status`
//! already set, then `comms_event` insert (decision 9). `id`-idempotent via
//! `ON CONFLICT DO NOTHING` on each table's own key -- `AuditRecord` carries
//! the same `comms_request_id`/`created_at` on every retry (`buffer::drain`
//! never mints new ones), so a retried write of an already-landed record is
//! a safe no-op rather than a duplicate.

use sqlx::PgPool;

use super::model::{AuditRecord, CHANNEL, CLASS, TEMPLATE_ID, TEMPLATE_VERSION};

pub async fn write_audit_record(
    pool: &PgPool,
    record: &AuditRecord,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;

    sqlx::query(
        r#"
        INSERT INTO comms_request (
            tenant_id, id, created_at, customer_id, channel, class, template_id,
            template_version, campaign_id, destination_hmac, destination_ciphertext,
            payload_ciphertext, producer_id, scheduled_for, expires_at,
            final_status, finalized_at
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, NULL, $9, $10, NULL, $11, NULL, NULL, $12, $3
        )
        ON CONFLICT (created_at, id) DO NOTHING
        "#,
    )
    .bind(record.tenant_id)
    .bind(record.comms_request_id)
    .bind(record.created_at)
    .bind(record.customer_id)
    .bind(CHANNEL)
    .bind(CLASS)
    .bind(TEMPLATE_ID)
    .bind(TEMPLATE_VERSION)
    .bind(&record.destination_hmac)
    .bind(&record.destination_ciphertext)
    .bind(record.producer_id)
    .bind(&record.final_status)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO comms_event (
            comms_request_id, customer_id, occurred_at, event_type, provider_ref,
            provider_status, provider_payload_ciphertext
        ) VALUES ($1, $2, $3, $4, $5, $6, NULL)
        ON CONFLICT (occurred_at, comms_request_id, event_type, provider_ref) DO NOTHING
        "#,
    )
    .bind(record.comms_request_id)
    .bind(record.customer_id)
    .bind(record.created_at)
    .bind(&record.final_status)
    .bind(&record.provider_ref)
    .bind(&record.provider_status)
    .execute(&mut *tx)
    .await?;

    tx.commit().await
}

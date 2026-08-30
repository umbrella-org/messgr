//! `producer_cert` (DESIGN.md §4.9) lives in the **control** database, not
//! the tenant database `producer` rows live in. mTLS resolution happens
//! before any tenant database is opened — the control database is the only
//! thing connected at that point, so the cert → tenant mapping has to live
//! there too (decision 1, T-005). `producer` itself carries no `tenant_id`
//! for the same reason in reverse: once a tenant's own database is open, the
//! tenant already *is* that database (§2.1).

use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProducerCert {
    pub cert_subject: String,
    pub tenant_id: Uuid,
    pub producer_id: Uuid,
}

pub async fn find_producer_cert(
    pool: &PgPool,
    cert_subject: &str,
) -> Result<Option<ProducerCert>, sqlx::Error> {
    sqlx::query_as::<_, ProducerCert>(
        "SELECT cert_subject, tenant_id, producer_id FROM producer_cert WHERE cert_subject = $1",
    )
    .bind(cert_subject)
    .fetch_optional(pool)
    .await
}

/// Inserts or re-confirms the `(cert_subject) -> (tenant_id, producer_id)`
/// mapping. Callers must have already checked `find_producer_cert` for a
/// conflicting existing row (decision 3, T-005) — this always writes the
/// given values, so it is idempotent only when called with the same
/// `(tenant_id, producer_id)` every time for a given `cert_subject`.
pub async fn upsert_producer_cert(
    pool: &PgPool,
    cert_subject: &str,
    tenant_id: Uuid,
    producer_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO producer_cert (cert_subject, tenant_id, producer_id)
        VALUES ($1, $2, $3)
        ON CONFLICT (cert_subject) DO UPDATE SET tenant_id = EXCLUDED.tenant_id, producer_id = EXCLUDED.producer_id
        "#,
    )
    .bind(cert_subject)
    .bind(tenant_id)
    .bind(producer_id)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Test cleanup only — nothing in the register/disable operations deletes a
/// `producer_cert` row (decision 5, T-005: disable never removes the
/// mapping).
pub async fn delete_producer_cert(
    pool: &PgPool,
    cert_subject: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM producer_cert WHERE cert_subject = $1")
        .bind(cert_subject)
        .execute(pool)
        .await
        .map(|_| ())
}

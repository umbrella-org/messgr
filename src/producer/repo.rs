use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

use super::model::Producer;

pub async fn find_by_name(
    pool: &PgPool,
    name: &str,
) -> Result<Option<Producer>, sqlx::Error> {
    sqlx::query_as::<_, Producer>(
        r#"
        SELECT id, name, cert_subject, owner_team, contact, enabled, created_at
        FROM producer
        WHERE name = $1
        "#,
    )
    .bind(name)
    .fetch_optional(pool)
    .await
}

pub async fn find_by_cert_subject(
    pool: &PgPool,
    cert_subject: &str,
) -> Result<Option<Producer>, sqlx::Error> {
    sqlx::query_as::<_, Producer>(
        r#"
        SELECT id, name, cert_subject, owner_team, contact, enabled, created_at
        FROM producer
        WHERE cert_subject = $1
        "#,
    )
    .bind(cert_subject)
    .fetch_optional(pool)
    .await
}

pub async fn list(pool: &PgPool) -> Result<Vec<Producer>, sqlx::Error> {
    sqlx::query_as::<_, Producer>(
        r#"
        SELECT id, name, cert_subject, owner_team, contact, enabled, created_at
        FROM producer
        ORDER BY name
        "#,
    )
    .fetch_all(pool)
    .await
}

/// Inserts a new producer row. Callers must have already checked
/// `find_by_name` — this is not itself idempotent, since a second insert for
/// the same `name` would violate the `UNIQUE` constraint;
/// `register_producer` (src/producer/register.rs) is what makes the overall
/// operation safe to repeat.
pub async fn insert(
    pool: &PgPool,
    id: Uuid,
    name: &str,
    cert_subject: &str,
    owner_team: &str,
    contact: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO producer (id, name, cert_subject, owner_team, contact, enabled, created_at)
        VALUES ($1, $2, $3, $4, $5, true, $6)
        "#,
    )
    .bind(id)
    .bind(name)
    .bind(cert_subject)
    .bind(owner_team)
    .bind(contact)
    .bind(Utc::now())
    .execute(pool)
    .await
    .map(|_| ())
}

pub async fn set_enabled(
    pool: &PgPool,
    producer_id: Uuid,
    enabled: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE producer SET enabled = $1 WHERE id = $2")
        .bind(enabled)
        .bind(producer_id)
        .execute(pool)
        .await
        .map(|_| ())
}

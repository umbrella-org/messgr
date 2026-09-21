use chrono::Utc;
use sqlx::PgPool;
use sqlx::postgres::PgTransaction;
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

/// Same as `find_by_name`, but inside a caller-owned transaction (T-026) —
/// see `register::classify_registration` for why this needs to share one
/// snapshot with `find_by_cert_subject_tx`.
pub async fn find_by_name_tx(
    tx: &mut PgTransaction<'_>,
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
    .fetch_optional(&mut **tx)
    .await
}

/// Looks up a producer by `cert_subject` inside a caller-owned transaction
/// (T-026) — see `register::classify_registration` for why this needs to
/// share one snapshot with `find_by_name_tx`. No non-transactional sibling:
/// every caller needs that shared snapshot.
pub async fn find_by_cert_subject_tx(
    tx: &mut PgTransaction<'_>,
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
    .fetch_optional(&mut **tx)
    .await
}

/// Resolves a producer id to its row — the admin panel's disable route
/// (T-049) names a producer by id, but `disable_producer_inner` (T-005) is
/// name-shaped like the rest of the CLI-era API; this bridges the two
/// without duplicating `disable_producer_inner`'s control/tenant audit
/// logic.
pub async fn find_by_id(
    pool: &PgPool,
    id: Uuid,
) -> Result<Option<Producer>, sqlx::Error> {
    sqlx::query_as::<_, Producer>(
        r#"
        SELECT id, name, cert_subject, owner_team, contact, enabled, created_at
        FROM producer
        WHERE id = $1
        "#,
    )
    .bind(id)
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

/// Inserts a new producer row unless `name` or `cert_subject` already
/// exists — an untargeted `ON CONFLICT DO NOTHING`, since either of
/// `producer`'s two independent `UNIQUE` constraints can be the one a
/// losing racer collides on and the caller cannot know which in advance
/// (T-026). Returns whether the row was actually inserted;
/// `register_producer` (src/producer/register.rs) is what reclassifies a
/// `false` into the correct idempotent/rejected outcome rather than
/// surfacing a raw constraint-violation error.
pub async fn insert_if_absent(
    pool: &PgPool,
    id: Uuid,
    name: &str,
    cert_subject: &str,
    owner_team: &str,
    contact: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        r#"
        INSERT INTO producer (id, name, cert_subject, owner_team, contact, enabled, created_at)
        VALUES ($1, $2, $3, $4, $5, true, $6)
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(id)
    .bind(name)
    .bind(cert_subject)
    .bind(owner_team)
    .bind(contact)
    .bind(Utc::now())
    .execute(pool)
    .await?;

    Ok(result.rows_affected() == 1)
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

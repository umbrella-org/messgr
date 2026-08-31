use chrono::{DateTime, Utc};
use sqlx::PgPool;

use super::model::Template;

pub async fn find(
    pool: &PgPool,
    template_id: &str,
    version: i32,
    locale: &str,
) -> Result<Option<Template>, sqlx::Error> {
    sqlx::query_as::<_, Template>(
        r#"
        SELECT template_id, version, channel, locale, body, approved_by, approved_at
        FROM template
        WHERE template_id = $1 AND version = $2 AND locale = $3
        "#,
    )
    .bind(template_id)
    .bind(version)
    .bind(locale)
    .fetch_optional(pool)
    .await
}

pub async fn list_versions(
    pool: &PgPool,
    template_id: &str,
) -> Result<Vec<Template>, sqlx::Error> {
    sqlx::query_as::<_, Template>(
        r#"
        SELECT template_id, version, channel, locale, body, approved_by, approved_at
        FROM template
        WHERE template_id = $1
        ORDER BY version, locale
        "#,
    )
    .bind(template_id)
    .fetch_all(pool)
    .await
}

/// Inserts a new template row. Callers must have already checked `find` for
/// this `(template_id, version, locale)` — this is not itself idempotent,
/// since a second insert for the same primary key would violate it;
/// `approve_template` (src/template/approve.rs) is what makes the overall
/// operation reject cleanly, matching `producer::repo::insert`'s convention.
#[allow(clippy::too_many_arguments)]
pub async fn insert(
    pool: &PgPool,
    template_id: &str,
    version: i32,
    channel: &str,
    locale: &str,
    body: &str,
    approved_by: &str,
    approved_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO template (template_id, version, channel, locale, body, approved_by, approved_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(template_id)
    .bind(version)
    .bind(channel)
    .bind(locale)
    .bind(body)
    .bind(approved_by)
    .bind(approved_at)
    .execute(pool)
    .await
    .map(|_| ())
}

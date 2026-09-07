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

/// Inserts a new template row unless `(template_id, version, locale)` is
/// already approved — `ON CONFLICT DO NOTHING` against the table's own
/// primary key, matching `customer_dek::repo::insert_if_absent`'s
/// race-handling shape (T-026). Returns whether the row was actually
/// inserted; `approve_template` (`src/template/approve.rs`) is what turns a
/// `false` into a clean rejection rather than a raw constraint-violation
/// error.
#[allow(clippy::too_many_arguments)]
pub async fn insert_if_absent(
    pool: &PgPool,
    template_id: &str,
    version: i32,
    channel: &str,
    locale: &str,
    body: &str,
    approved_by: &str,
    approved_at: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        r#"
        INSERT INTO template (template_id, version, channel, locale, body, approved_by, approved_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (template_id, version, locale) DO NOTHING
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
    .await?;

    Ok(result.rows_affected() == 1)
}

use sqlx::PgPool;

use super::model::{ProviderConfig, ProviderConfigInput};

/// Lists a channel's provider list in failover order.
pub async fn list(
    pool: &PgPool,
    channel: &str,
) -> Result<Vec<ProviderConfig>, sqlx::Error> {
    sqlx::query_as::<_, ProviderConfig>(
        r#"
        SELECT channel, priority, provider, credential_path, rate_limit_per_sec
        FROM provider_config
        WHERE channel = $1
        ORDER BY priority
        "#,
    )
    .bind(channel)
    .fetch_all(pool)
    .await
}

/// Loads a single `(channel, priority)` row, if one exists — used by
/// `configure::set_provider_config` to decide created/updated/idempotent.
pub async fn load_one(
    pool: &PgPool,
    channel: &str,
    priority: i16,
) -> Result<Option<ProviderConfig>, sqlx::Error> {
    sqlx::query_as::<_, ProviderConfig>(
        r#"
        SELECT channel, priority, provider, credential_path, rate_limit_per_sec
        FROM provider_config
        WHERE channel = $1 AND priority = $2
        "#,
    )
    .bind(channel)
    .bind(priority)
    .fetch_optional(pool)
    .await
}

/// Inserts or overwrites one `(channel, priority)` row. Not itself
/// responsible for the created/updated/idempotent distinction —
/// `configure::set_provider_config` (which calls `load_one` first) decides
/// that and only calls this when the values actually differ or no row
/// exists.
pub async fn upsert(
    pool: &PgPool,
    input: &ProviderConfigInput,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO provider_config (channel, priority, provider, credential_path, rate_limit_per_sec)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (channel, priority) DO UPDATE SET
            provider = EXCLUDED.provider,
            credential_path = EXCLUDED.credential_path,
            rate_limit_per_sec = EXCLUDED.rate_limit_per_sec
        "#,
    )
    .bind(&input.channel)
    .bind(input.priority)
    .bind(&input.provider)
    .bind(&input.credential_path)
    .bind(input.rate_limit_per_sec)
    .execute(pool)
    .await
    .map(|_| ())
}

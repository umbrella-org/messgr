use sqlx::PgPool;

use super::model::{TenantConfig, TenantConfigInput};

/// Loads the tenant's typed configuration, if one has ever been set
/// (T-007 decision 4 — a fresh tenant has no row until an operator
/// configures one).
pub async fn load(pool: &PgPool) -> Result<Option<TenantConfig>, sqlx::Error> {
    sqlx::query_as::<_, TenantConfig>(
        r#"
        SELECT retention_years, default_timezone, default_locale, schedule_horizon_days,
               quota_day_boundary_tz, verification_mode,
               kill_switch_release_rate
        FROM tenant_config
        WHERE singleton
        "#,
    )
    .fetch_optional(pool)
    .await
}

/// Inserts or overwrites the tenant's single `tenant_config` row. Not
/// itself responsible for the created/updated/idempotent distinction —
/// `configure::set_tenant_config` (which calls `load` first) decides that
/// and only calls this when the values actually differ or no row exists.
pub async fn upsert(
    pool: &PgPool,
    input: &TenantConfigInput,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO tenant_config (
            singleton, retention_years, default_timezone, default_locale,
            schedule_horizon_days, quota_day_boundary_tz, verification_mode,
            kill_switch_release_rate
        )
        VALUES (true, $1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (singleton) DO UPDATE SET
            retention_years = EXCLUDED.retention_years,
            default_timezone = EXCLUDED.default_timezone,
            default_locale = EXCLUDED.default_locale,
            schedule_horizon_days = EXCLUDED.schedule_horizon_days,
            quota_day_boundary_tz = EXCLUDED.quota_day_boundary_tz,
            verification_mode = EXCLUDED.verification_mode,
            kill_switch_release_rate = EXCLUDED.kill_switch_release_rate
        "#,
    )
    .bind(input.retention_years)
    .bind(&input.default_timezone)
    .bind(&input.default_locale)
    .bind(input.schedule_horizon_days)
    .bind(&input.quota_day_boundary_tz)
    .bind(&input.verification_mode)
    .bind(input.kill_switch_release_rate)
    .execute(pool)
    .await
    .map(|_| ())
}

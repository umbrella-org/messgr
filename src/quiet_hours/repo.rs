use sqlx::PgPool;

use super::model::{
    DEFAULT_SCOPE, DEFAULT_SCOPE_KEY, QuietHoursPolicy, QuietHoursPolicyInput,
};

/// The one row this ticket ever reads (decision 1) -- `messgr-dispatcher`
/// startup and `quiet-hours show` both call this, never a scope-parameterized
/// lookup.
pub async fn load_default(
    pool: &PgPool,
) -> Result<Option<QuietHoursPolicy>, sqlx::Error> {
    sqlx::query_as::<_, QuietHoursPolicy>(
        "SELECT scope, scope_key, start_local, end_local FROM quiet_hours_policy \
         WHERE scope = $1 AND scope_key = $2",
    )
    .bind(DEFAULT_SCOPE)
    .bind(DEFAULT_SCOPE_KEY)
    .fetch_optional(pool)
    .await
}

pub async fn upsert_default(
    pool: &PgPool,
    input: &QuietHoursPolicyInput,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO quiet_hours_policy (scope, scope_key, start_local, end_local)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (scope, scope_key) DO UPDATE SET
            start_local = EXCLUDED.start_local,
            end_local = EXCLUDED.end_local
        "#,
    )
    .bind(DEFAULT_SCOPE)
    .bind(DEFAULT_SCOPE_KEY)
    .bind(input.start_local)
    .bind(input.end_local)
    .execute(pool)
    .await
    .map(|_| ())
}

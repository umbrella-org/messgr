use sqlx::PgPool;

use super::model::KillSwitch;

/// Every currently-engaged switch (`released_at IS NULL`) — the full set
/// `KillSwitchCache::refresh` diffs against its previous snapshot.
pub async fn list_active(pool: &PgPool) -> Result<Vec<KillSwitch>, sqlx::Error> {
    sqlx::query_as::<_, KillSwitch>(
        r#"
        SELECT id, scope, scope_key, on_queued, engaged_by, engaged_at, reason,
               released_by, released_at
        FROM kill_switch
        WHERE released_at IS NULL
        "#,
    )
    .fetch_all(pool)
    .await
}

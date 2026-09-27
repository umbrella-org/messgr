use sqlx::PgPool;
use uuid::Uuid;

use super::model::{PlatformKillSwitch, scope};

/// Every currently-engaged platform switch, oldest first — the platform
/// console's and `messgr-control platform-kill-switch list`'s view.
pub async fn list_active(
    pool: &PgPool,
) -> Result<Vec<PlatformKillSwitch>, sqlx::Error> {
    sqlx::query_as::<_, PlatformKillSwitch>(
        r#"
        SELECT id, scope, tenant_id, engaged_by, engaged_at, reason, released_by, released_at
        FROM platform_kill_switch
        WHERE released_at IS NULL
        ORDER BY engaged_at
        "#,
    )
    .fetch_all(pool)
    .await
}

/// Every currently-engaged switch that blocks `tenant_id`: region-wide rows
/// plus this tenant's own tenant-scope row. Read on every
/// `KillSwitchCache` refresh (dispatcher and ingest) and by the tenant
/// admin panel.
pub async fn list_active_for_tenant(
    pool: &PgPool,
    tenant_id: Uuid,
) -> Result<Vec<PlatformKillSwitch>, sqlx::Error> {
    sqlx::query_as::<_, PlatformKillSwitch>(
        r#"
        SELECT id, scope, tenant_id, engaged_by, engaged_at, reason, released_by, released_at
        FROM platform_kill_switch
        WHERE released_at IS NULL
          AND (scope = $1 OR (scope = $2 AND tenant_id = $3))
        ORDER BY engaged_at
        "#,
    )
    .bind(scope::PLATFORM)
    .bind(scope::TENANT)
    .bind(tenant_id)
    .fetch_all(pool)
    .await
}

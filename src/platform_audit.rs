//! Writes to `platform_audit` (DESIGN.md §4.11) for any platform-level
//! action. Provisioning (`src/tenant/provision.rs`) is the only caller
//! today; suspension and break-glass content access (§7.6, §11.4) call this
//! same function once those subsystems exist. Not tenant-scoped —
//! `tenant_id` is nullable because some platform actions (e.g. a
//! platform-wide kill switch) have none.

use chrono::Utc;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

pub async fn record(
    pool: &PgPool,
    actor: &str,
    action: &str,
    tenant_id: Option<Uuid>,
    detail: Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO platform_audit (id, actor, action, tenant_id, detail, at)
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(actor)
    .bind(action)
    .bind(tenant_id)
    .bind(detail)
    .bind(Utc::now())
    .execute(pool)
    .await
    .map(|_| ())
}

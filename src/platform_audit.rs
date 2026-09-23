//! Writes to `platform_audit` (DESIGN.md §4.11) for any platform-level
//! action. Provisioning (`src/tenant/provision.rs`) and tenant destroy
//! (`src/tenant/offboard.rs`, T-059) are today's callers; suspension and
//! break-glass content access (§7.6, §11.4) call this same function once
//! those subsystems exist. Not tenant-scoped — `tenant_id` is nullable
//! because some platform actions (e.g. a platform-wide kill switch) have
//! none.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use sqlx::postgres::PgTransaction;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PlatformAuditRow {
    pub id: Uuid,
    pub actor: String,
    pub action: String,
    pub tenant_id: Option<Uuid>,
    pub detail: Value,
    pub at: DateTime<Utc>,
}

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

/// `record`'s transactional form, for callers that must commit this row
/// atomically with the state change it's auditing (the platform console's
/// suspend handler) rather than risk a status change with no audit trail if
/// the second, separate write failed.
pub async fn record_tx(
    tx: &mut PgTransaction<'_>,
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
    .execute(&mut **tx)
    .await
    .map(|_| ())
}

/// Newest first, optionally filtered by `tenant_id`/`action` -- the
/// platform console's audit view (T-057). `action` matches exactly, not a
/// substring search: the action strings this table holds are a small,
/// known vocabulary (`tenant.suspend`, `tenant.offboard_destroy`, ...), not
/// free text.
pub async fn list(
    pool: &PgPool,
    tenant_id: Option<Uuid>,
    action: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<PlatformAuditRow>, sqlx::Error> {
    sqlx::query_as::<_, PlatformAuditRow>(
        r#"
        SELECT id, actor, action, tenant_id, detail, at
        FROM platform_audit
        WHERE ($1::uuid IS NULL OR tenant_id = $1)
          AND ($2::text IS NULL OR action = $2)
        ORDER BY at DESC
        LIMIT $3 OFFSET $4
        "#,
    )
    .bind(tenant_id)
    .bind(action)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
}

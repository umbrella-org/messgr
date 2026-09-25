//! Engage/release for platform switches (T-058), plus the per-tenant
//! `NOTIFY kill_switch` fan-out DESIGN.md §5.2 prescribes: "the control
//! plane must fan out to each tenant database rather than issuing one
//! notification, so the dispatcher's periodic re-read (30s) is the
//! guaranteed path and `NOTIFY` is the fast path." Mirrors
//! `kill_switch::configure`'s shape; every outcome is audited in
//! `platform_audit`, in the same transaction as the write it records (T-057
//! F1's rule).

use sqlx::PgPool;
use sqlx::postgres::PgTransaction;
use uuid::Uuid;

use super::model::scope;
use crate::tenant::model::{Tenant, status};
use crate::tenant::pool::connect_tenant_pool;
use crate::tenant::repo as tenant_repo;

/// The fan-out opens one short-lived connection per tenant — an admin
/// action, not a hot path, so no persistent pool (T-058 decision 8).
const FAN_OUT_MAX_CONNECTIONS: u32 = 1;

/// The tenant-database channel `messgr-dispatcher` already `LISTEN`s on
/// (`migrations/tenant/0009_kill_switch.sql`) — deliberately not a second
/// channel (T-058 decision 2).
const NOTIFY_SQL: &str = "NOTIFY kill_switch";

#[derive(Debug)]
pub enum ConfigureError {
    Database(sqlx::Error),
    /// A live switch already covers this exact `(scope, tenant_id)` — the
    /// `platform_kill_switch_live_idx` conflict, mapped to a domain outcome.
    AlreadyEngaged,
    /// An unrecognised scope, a scope/tenant pairing the table's CHECK would
    /// refuse, or an unknown tenant — refused (and audited) before any write.
    Rejected(String),
}

impl std::fmt::Display for ConfigureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(err) => {
                write!(f, "platform_kill_switch operation failed: {err}")
            }
            Self::AlreadyEngaged => {
                write!(f, "a platform switch with this scope is already engaged")
            }
            Self::Rejected(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for ConfigureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(err) => Some(err),
            Self::AlreadyEngaged | Self::Rejected(_) => None,
        }
    }
}

impl From<sqlx::Error> for ConfigureError {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err)
    }
}

/// What `engage_tx` wrote. `notify_targets` are the tenants whose databases
/// must be sent `NOTIFY kill_switch` once the caller's transaction commits
/// — never before, or a dispatcher woken early re-reads the control
/// database and sees nothing yet.
#[derive(Debug)]
pub struct EngageOutcome {
    pub id: Uuid,
    pub notify_targets: Vec<Tenant>,
}

#[derive(Debug)]
pub struct ReleaseOutcome {
    /// "released" | "idempotent" (already released, or unknown id —
    /// `kill_switch::configure::release`'s pattern).
    pub outcome: &'static str,
    pub notify_targets: Vec<Tenant>,
}

/// How the best-effort fan-out went. A failure is logged and counted, never
/// returned as an error: the engage/release has already committed, and the
/// dispatcher's 30s poll and ingest's 5s poll pick it up regardless.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FanOutReport {
    pub notified: usize,
    pub failed: usize,
}

/// Engages a switch inside the caller's transaction, auditing the outcome
/// in that same transaction. Leaves `tx` committable on every domain
/// outcome — validation runs before any write, and a duplicate is caught by
/// `ON CONFLICT DO NOTHING` rather than a unique violation that would abort
/// the transaction — so a caller that treats `AlreadyEngaged` as fine
/// (T-057's suspend) can still commit its own work, and a caller that
/// returns the error can commit the audit row. Only `Database` means the
/// transaction must be rolled back.
pub async fn engage_tx(
    tx: &mut PgTransaction<'_>,
    scope_value: &str,
    tenant_id: Option<Uuid>,
    reason: &str,
    actor: &str,
) -> Result<EngageOutcome, ConfigureError> {
    let rejection = match (scope_value, tenant_id) {
        (scope::PLATFORM, None) => None,
        (scope::PLATFORM, Some(_)) => {
            Some("a platform-scope switch must not name a tenant".to_string())
        }
        (scope::TENANT, None) => {
            Some("a tenant-scope switch must name a tenant".to_string())
        }
        (scope::TENANT, Some(id)) => {
            if tenant_repo::find_by_id(&mut **tx, id).await?.is_none() {
                Some(format!("no tenant registered with id {id}"))
            } else {
                None
            }
        }
        _ => Some(format!(
            "unrecognised platform_kill_switch scope {scope_value:?}"
        )),
    };
    if let Some(message) = rejection {
        audit_engage(tx, actor, scope_value, tenant_id, reason, "rejected").await?;
        return Err(ConfigureError::Rejected(message));
    }

    let id = Uuid::new_v4();
    let inserted: Option<Uuid> = sqlx::query_scalar(
        r#"
        INSERT INTO platform_kill_switch (id, scope, tenant_id, engaged_by, engaged_at, reason)
        VALUES ($1, $2, $3, $4, now(), $5)
        ON CONFLICT (scope, COALESCE(tenant_id, '00000000-0000-0000-0000-000000000000'::uuid))
            WHERE released_at IS NULL
            DO NOTHING
        RETURNING id
        "#,
    )
    .bind(id)
    .bind(scope_value)
    .bind(tenant_id)
    .bind(actor)
    .bind(reason)
    .fetch_optional(&mut **tx)
    .await?;

    if inserted.is_none() {
        audit_engage(tx, actor, scope_value, tenant_id, reason, "already_engaged")
            .await?;
        return Err(ConfigureError::AlreadyEngaged);
    }

    audit_engage(tx, actor, scope_value, tenant_id, reason, "engaged").await?;
    let notify_targets = fan_out_targets(tx, scope_value, tenant_id).await?;
    Ok(EngageOutcome { id, notify_targets })
}

/// `engage_tx` in its own transaction, followed by the fan-out. A rejected
/// or duplicate engage still commits its audit row before returning the
/// error.
pub async fn engage(
    control_pool: &PgPool,
    base_db_url: &str,
    scope_value: &str,
    tenant_id: Option<Uuid>,
    reason: &str,
    actor: &str,
) -> Result<(EngageOutcome, FanOutReport), ConfigureError> {
    let mut tx = control_pool.begin().await?;
    match engage_tx(&mut tx, scope_value, tenant_id, reason, actor).await {
        Ok(outcome) => {
            tx.commit().await?;
            let report =
                notify_tenants(control_pool, base_db_url, &outcome.notify_targets)
                    .await;
            Ok((outcome, report))
        }
        Err(ConfigureError::Database(err)) => Err(err.into()),
        Err(domain) => {
            tx.commit().await?;
            Err(domain)
        }
    }
}

async fn audit_engage(
    tx: &mut PgTransaction<'_>,
    actor: &str,
    scope_value: &str,
    tenant_id: Option<Uuid>,
    reason: &str,
    outcome: &str,
) -> Result<(), sqlx::Error> {
    crate::platform_audit::record_tx(
        tx,
        actor,
        "platform_kill_switch.engage",
        tenant_id,
        serde_json::json!({
            "scope": scope_value,
            "reason": reason,
            "outcome": outcome,
        }),
    )
    .await
}

/// Releases a switch (idempotent — releasing an already-released or unknown
/// id succeeds and still audits), then fans out. A released switch's held
/// backlog is not dispatched in one burst: the dispatcher sees it in
/// `RefreshDelta::released` and ramps it back in (DESIGN.md §5.2).
pub async fn release(
    control_pool: &PgPool,
    base_db_url: &str,
    id: Uuid,
    actor: &str,
) -> Result<(ReleaseOutcome, FanOutReport), ConfigureError> {
    let mut tx = control_pool.begin().await?;

    let released: Option<(String, Option<Uuid>)> = sqlx::query_as(
        "UPDATE platform_kill_switch SET released_by = $1, released_at = now() \
         WHERE id = $2 AND released_at IS NULL RETURNING scope, tenant_id",
    )
    .bind(actor)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;

    let (outcome, tenant_id, notify_targets) = match released {
        Some((scope_value, tenant_id)) => {
            let targets = fan_out_targets(&mut tx, &scope_value, tenant_id).await?;
            ("released", tenant_id, targets)
        }
        None => ("idempotent", None, Vec::new()),
    };

    crate::platform_audit::record_tx(
        &mut tx,
        actor,
        "platform_kill_switch.release",
        tenant_id,
        serde_json::json!({ "id": id, "outcome": outcome }),
    )
    .await?;
    tx.commit().await?;

    let report = notify_tenants(control_pool, base_db_url, &notify_targets).await;
    Ok((
        ReleaseOutcome {
            outcome,
            notify_targets,
        },
        report,
    ))
}

/// The tenants a switch of this scope reaches: the one named tenant, or,
/// region-wide, every tenant whose database still exists. A `provisioning`
/// tenant has no dispatcher yet and an `offboarding_destroy` tenant's
/// database has been dropped (T-059), so neither is notified; a `suspended`
/// or `offboarding_archive` tenant's dispatcher may still be running.
async fn fan_out_targets(
    tx: &mut PgTransaction<'_>,
    scope_value: &str,
    tenant_id: Option<Uuid>,
) -> Result<Vec<Tenant>, sqlx::Error> {
    match (scope_value, tenant_id) {
        (scope::TENANT, Some(id)) => Ok(tenant_repo::find_by_id(&mut **tx, id)
            .await?
            .into_iter()
            .collect()),
        (scope::PLATFORM, _) => {
            let tenants = tenant_repo::list(&mut **tx).await?;
            Ok(tenants
                .into_iter()
                .filter(|t| {
                    matches!(
                        t.status.as_str(),
                        status::ACTIVE
                            | status::SUSPENDED
                            | status::OFFBOARDING_ARCHIVE
                    )
                })
                .collect())
        }
        _ => Ok(Vec::new()),
    }
}

/// Sends `NOTIFY kill_switch` on each target tenant's own database. Best
/// effort by design (T-058 decision 8): a tenant whose database is
/// unreachable is logged and counted, and still picks the change up on its
/// next poll.
pub async fn notify_tenants(
    control_pool: &PgPool,
    base_db_url: &str,
    targets: &[Tenant],
) -> FanOutReport {
    let mut report = FanOutReport::default();
    for tenant in targets {
        match notify_one(control_pool, base_db_url, tenant).await {
            Ok(()) => report.notified += 1,
            Err(err) => {
                report.failed += 1;
                tracing::warn!(
                    tenant_id = %tenant.id,
                    %err,
                    "platform_kill_switch: NOTIFY fan-out failed for tenant; \
                     its dispatcher will pick the change up on its next 30s poll"
                );
            }
        }
    }
    report
}

async fn notify_one(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant: &Tenant,
) -> Result<(), sqlx::Error> {
    let tenant_pool = connect_tenant_pool(
        control_pool,
        base_db_url,
        tenant.id,
        &tenant.database_name,
        FAN_OUT_MAX_CONNECTIONS,
    )
    .await?;
    let result = sqlx::query(NOTIFY_SQL).execute(&tenant_pool.pool).await;
    tenant_pool.pool.close().await;
    result.map(|_| ())
}

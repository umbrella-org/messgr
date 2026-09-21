//! Engage/release mutations (T-049 decision 4) — the operator-facing panel
//! T-016 deliberately left to a `psql` runbook. Mirrors
//! `producer_quota::configure`'s shape: a `ConfigureError`/outcome enum, an
//! `audit_*` helper calling `platform_audit::record` on every outcome.

use sqlx::PgPool;
use uuid::Uuid;

use super::model::{on_queued, scope};

/// The unique index `migrations/tenant/0009_kill_switch.sql` declares on
/// `(scope, COALESCE(scope_key, '')) WHERE released_at IS NULL` — unnamed in
/// the migration, so Postgres auto-generates this name; confirmed against
/// the current schema (T-049 applicability check).
const ALREADY_ENGAGED_CONSTRAINT: &str = "kill_switch_scope_coalesce_idx";

#[derive(Debug)]
pub enum ConfigureError {
    Database(sqlx::Error),
    /// A live switch already covers this exact `(scope, scope_key)` pair —
    /// the unique-index violation mapped to a domain outcome, not a raw
    /// `500`.
    AlreadyEngaged,
}

impl std::fmt::Display for ConfigureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(err) => write!(f, "kill_switch operation failed: {err}"),
            Self::AlreadyEngaged => {
                write!(f, "a switch with this scope is already engaged")
            }
        }
    }
}

impl std::error::Error for ConfigureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(err) => Some(err),
            Self::AlreadyEngaged => None,
        }
    }
}

impl From<sqlx::Error> for ConfigureError {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err)
    }
}

fn rejected(message: String) -> ConfigureError {
    sqlx::Error::Configuration(message.into()).into()
}

#[derive(Debug)]
pub struct EngageOutcome {
    pub id: Uuid,
    /// "engaged" — never "rejected"/"already_engaged", which are returned as
    /// an `Err` instead.
    pub outcome: &'static str,
}

#[derive(Debug)]
pub struct ReleaseOutcome {
    /// "released" | "idempotent" (already released, or unknown id — T-041's
    /// `cancel` pattern: no way to tell those apart from `id` alone, and
    /// both are a safe no-op response).
    pub outcome: &'static str,
}

/// Engages a new switch. Rejects (audited) an unrecognised `scope` or
/// `on_queued` value before touching the database; maps a live switch
/// already covering this `(scope, scope_key)` to `AlreadyEngaged` (audited)
/// rather than surfacing the raw constraint violation.
#[allow(clippy::too_many_arguments)]
pub async fn engage(
    pool: &PgPool,
    control_pool: &PgPool,
    tenant_id: Uuid,
    scope_value: &str,
    scope_key: Option<&str>,
    on_queued_value: &str,
    reason: &str,
    actor: &str,
) -> Result<EngageOutcome, ConfigureError> {
    let valid_scope = matches!(
        scope_value,
        scope::GLOBAL
            | scope::CHANNEL
            | scope::PRODUCER
            | scope::PRODUCER_CHANNEL
            | scope::CAMPAIGN
    );
    let valid_on_queued =
        matches!(on_queued_value, on_queued::HOLD | on_queued::DISCARD);

    if !valid_scope || !valid_on_queued {
        audit_engage(
            control_pool,
            actor,
            tenant_id,
            scope_value,
            scope_key,
            on_queued_value,
            reason,
            "rejected",
        )
        .await?;

        let message = if !valid_scope {
            format!("unrecognised kill_switch scope {scope_value:?}")
        } else {
            format!("unrecognised kill_switch on_queued value {on_queued_value:?}")
        };
        return Err(rejected(message));
    }

    let id = Uuid::new_v4();
    let result = sqlx::query(
        r#"
        INSERT INTO kill_switch (id, scope, scope_key, on_queued, engaged_by, engaged_at, reason, released_by, released_at)
        VALUES ($1, $2, $3, $4, $5, now(), $6, NULL, NULL)
        "#,
    )
    .bind(id)
    .bind(scope_value)
    .bind(scope_key)
    .bind(on_queued_value)
    .bind(actor)
    .bind(reason)
    .execute(pool)
    .await;

    match result {
        Ok(_) => {
            audit_engage(
                control_pool,
                actor,
                tenant_id,
                scope_value,
                scope_key,
                on_queued_value,
                reason,
                "engaged",
            )
            .await?;

            Ok(EngageOutcome {
                id,
                outcome: "engaged",
            })
        }
        Err(sqlx::Error::Database(db_err))
            if db_err.constraint() == Some(ALREADY_ENGAGED_CONSTRAINT) =>
        {
            audit_engage(
                control_pool,
                actor,
                tenant_id,
                scope_value,
                scope_key,
                on_queued_value,
                reason,
                "already_engaged",
            )
            .await?;

            Err(ConfigureError::AlreadyEngaged)
        }
        Err(err) => Err(err.into()),
    }
}

#[allow(clippy::too_many_arguments)]
async fn audit_engage(
    control_pool: &PgPool,
    actor: &str,
    tenant_id: Uuid,
    scope_value: &str,
    scope_key: Option<&str>,
    on_queued_value: &str,
    reason: &str,
    outcome: &str,
) -> Result<(), sqlx::Error> {
    crate::platform_audit::record(
        control_pool,
        actor,
        "kill_switch.engage",
        Some(tenant_id),
        serde_json::json!({
            "scope": scope_value,
            "scope_key": scope_key,
            "on_queued": on_queued_value,
            "reason": reason,
            "outcome": outcome,
        }),
    )
    .await
}

/// Releases an engaged switch (idempotent — releasing an already-released or
/// unknown id succeeds and still audits, T-041 `cancel`'s pattern).
pub async fn release(
    pool: &PgPool,
    control_pool: &PgPool,
    tenant_id: Uuid,
    id: Uuid,
    actor: &str,
) -> Result<ReleaseOutcome, ConfigureError> {
    let released: Option<Uuid> = sqlx::query_scalar(
        "UPDATE kill_switch SET released_by = $1, released_at = now() \
         WHERE id = $2 AND released_at IS NULL RETURNING id",
    )
    .bind(actor)
    .bind(id)
    .fetch_optional(pool)
    .await?;

    let outcome = if released.is_some() {
        "released"
    } else {
        "idempotent"
    };

    crate::platform_audit::record(
        control_pool,
        actor,
        "kill_switch.release",
        Some(tenant_id),
        serde_json::json!({ "id": id, "outcome": outcome }),
    )
    .await?;

    Ok(ReleaseOutcome { outcome })
}

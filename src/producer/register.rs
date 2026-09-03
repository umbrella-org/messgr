use sqlx::PgPool;
use uuid::Uuid;

use crate::tenant::pool::connect_tenant_pool;
use crate::tenant::repo as tenant_repo;

use super::cert_repo;
use super::model::Producer;
use super::repo;

/// A register/disable run can fail on either the control-database side, the
/// tenant-database side, or a domain-level rejection (decision 3, T-005) —
/// the last of these is carried as `sqlx::Error::Configuration`, in the
/// shape `provision_tenant`'s own rejected branch already uses.
#[derive(Debug)]
pub enum ProducerError {
    Database(sqlx::Error),
}

impl std::fmt::Display for ProducerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(err) => write!(f, "producer operation failed: {err}"),
        }
    }
}

impl std::error::Error for ProducerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(err) => Some(err),
        }
    }
}

impl From<sqlx::Error> for ProducerError {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err)
    }
}

fn rejected(message: String) -> ProducerError {
    sqlx::Error::Configuration(message.into()).into()
}

#[derive(Debug)]
pub struct RegisterOutcome {
    pub producer_id: Uuid,
    /// "created" | "idempotent" — never "rejected", which is returned as an
    /// `Err` instead.
    pub outcome: &'static str,
}

#[derive(Debug)]
pub struct DisableOutcome {
    /// "disabled" | "idempotent" — never "rejected", which is returned as an
    /// `Err` instead.
    pub outcome: &'static str,
}

/// Registers a producer against a tenant (DESIGN.md §4.9, decision 1, T-005):
/// writes the tenant `producer` row, then the control `producer_cert`
/// mapping (decision 2's crash-window ordering), and writes exactly one
/// `platform_audit` row on every outcome (decision 4) — created, idempotent,
/// or rejected.
///
/// Idempotent on an identical `(name, cert_subject, owner_team, contact)` for
/// an existing `name` (decision 3); rejected when `name` already exists with
/// different inputs, when `cert_subject` is already bound to a different
/// producer in this tenant, or when `cert_subject` is already bound to a
/// different tenant entirely (the cross-tenant impersonation case).
#[allow(clippy::too_many_arguments)]
pub async fn register_producer(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    name: &str,
    cert_subject: &str,
    owner_team: &str,
    contact: &str,
    actor: &str,
) -> Result<RegisterOutcome, ProducerError> {
    let tenant = match tenant_repo::find_by_slug(control_pool, tenant_slug).await? {
        Some(tenant) => tenant,
        None => {
            audit(
                control_pool,
                actor,
                "producer.register",
                None,
                name,
                Some(cert_subject),
                "rejected",
            )
            .await?;

            return Err(rejected(format!(
                "no tenant registered with slug {tenant_slug:?}"
            )));
        }
    };

    let tenant_pool = connect_tenant_pool(
        control_pool,
        base_db_url,
        tenant.id,
        &tenant.database_name,
        5,
    )
    .await?;

    let result = register_producer_inner(
        control_pool,
        &tenant_pool.pool,
        tenant.id,
        name,
        cert_subject,
        owner_team,
        contact,
        actor,
    )
    .await;

    tenant_pool.pool.close().await;
    result
}

#[allow(clippy::too_many_arguments)]
async fn register_producer_inner(
    control_pool: &PgPool,
    tenant_pool: &PgPool,
    tenant_id: Uuid,
    name: &str,
    cert_subject: &str,
    owner_team: &str,
    contact: &str,
    actor: &str,
) -> Result<RegisterOutcome, ProducerError> {
    if let Some(existing) = repo::find_by_name(tenant_pool, name).await? {
        if existing.cert_subject == cert_subject
            && existing.owner_team == owner_team
            && existing.contact == contact
        {
            cert_repo::upsert_producer_cert(
                control_pool,
                cert_subject,
                tenant_id,
                existing.id,
            )
            .await?;

            audit(
                control_pool,
                actor,
                "producer.register",
                Some(tenant_id),
                name,
                Some(cert_subject),
                "idempotent",
            )
            .await?;

            return Ok(RegisterOutcome {
                producer_id: existing.id,
                outcome: "idempotent",
            });
        }

        audit(
            control_pool,
            actor,
            "producer.register",
            Some(tenant_id),
            name,
            Some(cert_subject),
            "rejected",
        )
        .await?;

        return Err(rejected(format!(
            "producer {name:?} is already registered with cert_subject={:?} owner_team={:?} contact={:?}; \
             refusing to re-register it with different inputs \
             (registration is idempotent only when re-run with identical inputs)",
            existing.cert_subject, existing.owner_team, existing.contact,
        )));
    }

    if let Some(other) = repo::find_by_cert_subject(tenant_pool, cert_subject).await? {
        audit(
            control_pool,
            actor,
            "producer.register",
            Some(tenant_id),
            name,
            Some(cert_subject),
            "rejected",
        )
        .await?;

        return Err(rejected(format!(
            "cert_subject {cert_subject:?} is already registered to producer {:?} in this tenant",
            other.name,
        )));
    }

    if let Some(existing_cert) =
        cert_repo::find_producer_cert(control_pool, cert_subject).await?
        && existing_cert.tenant_id != tenant_id
    {
        audit(
            control_pool,
            actor,
            "producer.register",
            Some(tenant_id),
            name,
            Some(cert_subject),
            "rejected",
        )
        .await?;

        return Err(rejected(format!(
            "cert_subject {cert_subject:?} is already registered to a different tenant"
        )));
    }

    let producer_id = Uuid::new_v4();
    repo::insert(
        tenant_pool,
        producer_id,
        name,
        cert_subject,
        owner_team,
        contact,
    )
    .await?;
    cert_repo::upsert_producer_cert(control_pool, cert_subject, tenant_id, producer_id)
        .await?;

    audit(
        control_pool,
        actor,
        "producer.register",
        Some(tenant_id),
        name,
        Some(cert_subject),
        "created",
    )
    .await?;

    Ok(RegisterOutcome {
        producer_id,
        outcome: "created",
    })
}

/// Disables a producer (decision 5, T-005): sets `enabled = false` on the
/// tenant row and leaves the control `producer_cert` mapping in place, so
/// T-006's mTLS resolution can tell "unknown cert" from "known but
/// disabled". Idempotent — disabling an already-disabled producer succeeds
/// and still audits.
pub async fn disable_producer(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    name: &str,
    actor: &str,
) -> Result<DisableOutcome, ProducerError> {
    let tenant = match tenant_repo::find_by_slug(control_pool, tenant_slug).await? {
        Some(tenant) => tenant,
        None => {
            audit(
                control_pool,
                actor,
                "producer.disable",
                None,
                name,
                None,
                "rejected",
            )
            .await?;

            return Err(rejected(format!(
                "no tenant registered with slug {tenant_slug:?}"
            )));
        }
    };

    let tenant_pool = connect_tenant_pool(
        control_pool,
        base_db_url,
        tenant.id,
        &tenant.database_name,
        5,
    )
    .await?;

    let result =
        disable_producer_inner(control_pool, &tenant_pool.pool, tenant.id, name, actor)
            .await;

    tenant_pool.pool.close().await;
    result
}

async fn disable_producer_inner(
    control_pool: &PgPool,
    tenant_pool: &PgPool,
    tenant_id: Uuid,
    name: &str,
    actor: &str,
) -> Result<DisableOutcome, ProducerError> {
    let existing = match repo::find_by_name(tenant_pool, name).await? {
        Some(existing) => existing,
        None => {
            audit(
                control_pool,
                actor,
                "producer.disable",
                Some(tenant_id),
                name,
                None,
                "rejected",
            )
            .await?;

            return Err(rejected(format!(
                "no producer named {name:?} is registered for this tenant"
            )));
        }
    };

    // Control-first (T-006 decision 2 — the inverse of register's tenant-first
    // ordering, decision 2 of T-005): producer_cert.enabled is now the copy
    // mTLS resolution actually reads, so a crash between the two writes must
    // leave the fail-closed side landed first. Written on both the
    // first-disable and idempotent branches, matching `repo::set_enabled`
    // below.
    cert_repo::set_cert_enabled(control_pool, &existing.cert_subject, false).await?;

    let outcome = if existing.enabled {
        repo::set_enabled(tenant_pool, existing.id, false).await?;
        "disabled"
    } else {
        "idempotent"
    };

    audit(
        control_pool,
        actor,
        "producer.disable",
        Some(tenant_id),
        name,
        Some(&existing.cert_subject),
        outcome,
    )
    .await?;

    Ok(DisableOutcome { outcome })
}

pub async fn list_producers(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
) -> Result<Vec<Producer>, ProducerError> {
    let tenant = tenant_repo::find_by_slug(control_pool, tenant_slug)
        .await?
        .ok_or_else(|| {
            rejected(format!("no tenant registered with slug {tenant_slug:?}"))
        })?;

    let tenant_pool = connect_tenant_pool(
        control_pool,
        base_db_url,
        tenant.id,
        &tenant.database_name,
        5,
    )
    .await?;
    let producers = repo::list(&tenant_pool.pool).await;
    tenant_pool.pool.close().await;

    Ok(producers?)
}

async fn audit(
    control_pool: &PgPool,
    actor: &str,
    action: &str,
    tenant_id: Option<Uuid>,
    name: &str,
    cert_subject: Option<&str>,
    outcome: &str,
) -> Result<(), sqlx::Error> {
    crate::platform_audit::record(
        control_pool,
        actor,
        action,
        tenant_id,
        serde_json::json!({
            "name": name,
            "cert_subject": cert_subject,
            "outcome": outcome,
        }),
    )
    .await
}

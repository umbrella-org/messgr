//! Whole-tenant termination (T-059, DESIGN.md §7.6, §7.7): the inverse of
//! `tenant::provision::provision_tenant`. Destroys the tenant's Vault
//! Transit key first — the database becomes permanently unreadable
//! immediately, without touching a row (§7.6) — then drops the database,
//! then marks the tenant row `offboarding_destroy`. Terminate-and-archive
//! (database/key retained for the tenant's remaining retention period) is a
//! separate, unticketed follow-up pending the pricing decision (design-doc
//! still-open item #13) — not this module's concern.

use sqlx::{Executor, PgPool};
use uuid::Uuid;
use vaultrs::client::VaultClient;

use super::model::status;
use super::provision::ProvisionError;
use super::{repo, vault};

/// Destroys `tenant_id`'s Vault Transit key/mount/policy/AppRole, drops its
/// database, and marks it `offboarding_destroy`. Vault-first, always in that
/// order (decision 1): a failure partway through must never leave a live
/// decryption key behind while its database is gone or vice versa — Vault
/// going first means the worst case is a database that still exists but is
/// already unreadable. Safe to call twice (decision 3): `destroy_vault`
/// tolerates its resources already being gone, and `DROP DATABASE IF
/// EXISTS` is itself a no-op on a second call.
pub async fn destroy_tenant(
    control_pool: &PgPool,
    tenant_id: Uuid,
    actor: &str,
    vault_client: &VaultClient,
) -> Result<(), ProvisionError> {
    let tenant = repo::find_by_id(control_pool, tenant_id)
        .await?
        .ok_or(ProvisionError::Database(sqlx::Error::RowNotFound))?;

    vault::destroy_vault(vault_client, &tenant.slug).await?;

    // DROP DATABASE cannot run inside a transaction block and cannot be
    // parameterized — `database_name` comes from the `tenant` row, not user
    // input at this call site (the same trust boundary
    // `provision::ensure_database_exists` already relies on). `WITH (FORCE)`
    // (Postgres 13+) terminates any lingering connections atomically as part
    // of the drop (decision 2).
    let statement = format!(
        "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
        tenant.database_name
    );
    control_pool.execute(statement.as_str()).await?;

    repo::mark_status(control_pool, tenant_id, status::OFFBOARDING_DESTROY).await?;

    crate::platform_audit::record(
        control_pool,
        actor,
        "tenant.offboard_destroy",
        Some(tenant_id),
        serde_json::json!({
            "slug": tenant.slug,
            "database_name": tenant.database_name,
        }),
    )
    .await?;

    Ok(())
}

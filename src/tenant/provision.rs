use sqlx::{Executor, PgPool, Row};
use uuid::Uuid;

use crate::profile::Profile;

use super::{pool::connect_tenant_pool, repo};

/// Provisions a tenant end to end (DESIGN.md §11.4, minus the pieces that
/// don't exist yet — see the ticket's decision 2): ensures the `tenant` row
/// exists, creates the tenant's database if it doesn't already exist, runs
/// the tenant migrations against it, and records the applied schema version.
///
/// Safe to call twice for the same `slug` **with the same `region` and
/// `database_name`** (decision 8): a second call finds the existing tenant,
/// skips `CREATE DATABASE`, re-runs the (harmless, currently empty) tenant
/// migrations, and refreshes `tenant_schema_version`. A second call for the
/// same slug with a *different* `region` or `database_name` is rejected
/// (review finding T-001/F1) rather than silently provisioning a second,
/// orphaned database while the registry keeps pointing at the first one —
/// idempotence means repeating the same operation, not overwriting it with a
/// different one.
///
/// Every one of the three outcomes (created, idempotent no-op, rejected)
/// writes exactly one `platform_audit` row (DESIGN.md §4.11, review finding
/// T-001/F6) via `platform_audit::record`, tagged with `actor` — the
/// operator identity supplied by the caller, since `messgr-control` has no
/// auth realm to infer it from yet.
///
/// Out of scope, deliberately: creating the tenant's Vault Transit mount and
/// AppRole, and starting its dispatcher pair. Those subsystems don't exist
/// yet (§7.6 lands with encryption, §9 with the dispatcher); `vault_mount` is
/// still recorded now as the deterministic path it will occupy later.
pub async fn provision_tenant(
    control_pool: &PgPool,
    base_db_url: &str,
    slug: &str,
    region: &str,
    database_name: &str,
    profile: Profile,
    actor: &str,
) -> Result<Uuid, sqlx::Error> {
    let (tenant_id, outcome) = match repo::find_by_slug(control_pool, slug).await? {
        Some(tenant)
            if tenant.region == region && tenant.database_name == database_name =>
        {
            (tenant.id, "idempotent")
        }
        Some(tenant) => {
            crate::platform_audit::record(
                control_pool,
                actor,
                "tenant.provision_rejected",
                Some(tenant.id),
                serde_json::json!({
                    "slug": slug,
                    "attempted_region": region,
                    "attempted_database_name": database_name,
                    "existing_region": tenant.region,
                    "existing_database_name": tenant.database_name,
                }),
            )
            .await?;

            return Err(sqlx::Error::Configuration(
                format!(
                    "tenant {slug:?} is already registered with region={:?} database_name={:?}; \
                     refusing to re-provision it with region={region:?} database_name={database_name:?} \
                     (provisioning is idempotent only when re-run with identical inputs)",
                    tenant.region, tenant.database_name,
                )
                .into(),
            ));
        }
        None => {
            let id = Uuid::new_v4();
            let vault_mount = format!("transit/{slug}/messgr-dek");
            let webhook_token = Uuid::new_v4().to_string();

            repo::insert_provisioning(
                control_pool,
                id,
                slug,
                region,
                database_name,
                &vault_mount,
                &webhook_token,
            )
            .await?;

            (id, "created")
        }
    };

    ensure_database_exists(control_pool, database_name).await?;

    let tenant_pool =
        connect_tenant_pool(base_db_url, database_name, 5, profile).await?;
    // `migrate!`'s path is resolved relative to CARGO_MANIFEST_DIR, not this
    // file's location.
    sqlx::migrate!("./migrations/tenant")
        .run(&tenant_pool)
        .await
        .map_err(|err| sqlx::Error::Configuration(err.to_string().into()))?;

    let version = current_schema_version(&tenant_pool).await?;
    repo::record_schema_version(control_pool, tenant_id, version).await?;
    tenant_pool.close().await;

    repo::mark_active(control_pool, tenant_id).await?;

    crate::platform_audit::record(
        control_pool,
        actor,
        "tenant.provision",
        Some(tenant_id),
        serde_json::json!({
            "slug": slug,
            "region": region,
            "database_name": database_name,
            "outcome": outcome,
        }),
    )
    .await?;

    Ok(tenant_id)
}

async fn ensure_database_exists(
    control_pool: &PgPool,
    database_name: &str,
) -> Result<(), sqlx::Error> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)",
    )
    .bind(database_name)
    .fetch_one(control_pool)
    .await?;

    if !exists {
        // CREATE DATABASE cannot run inside a transaction block and cannot be
        // parameterized — the name is operator-supplied via the CLI, not
        // customer input, so this identifier is quoted rather than bound.
        let statement = format!("CREATE DATABASE \"{database_name}\"");
        control_pool.execute(statement.as_str()).await?;
    }

    Ok(())
}

async fn current_schema_version(tenant_pool: &PgPool) -> Result<i64, sqlx::Error> {
    let row = tenant_pool
        .fetch_optional("SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations")
        .await?;

    Ok(match row {
        Some(row) => row.get(0),
        None => 0,
    })
}

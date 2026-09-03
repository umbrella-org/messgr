use sqlx::PgPool;
use uuid::Uuid;

use crate::db;
use crate::tenant::repo as tenant_repo;

/// A tenant's pool, paired with the tenant identity it was opened for.
/// The only way to get one is `connect_tenant_pool` — there is no other
/// constructor, so a caller can never hold a `TenantPool` whose `tenant_id`
/// and `pool` came from different resolutions of "which tenant".
pub struct TenantPool {
    pub tenant_id: Uuid,
    pub pool: PgPool,
}

/// Connects a pool scoped to one tenant's database, derived from
/// `base_db_url` (any URL pointing at the same Postgres cluster — host,
/// port, and credentials are shared across tenants; only the database name
/// varies) by swapping in `database_name`. Wired with the §2.1
/// `current_database()` assertion via `db::connect_with_expected_database` —
/// but the assertion's `expected_db` comes from a *fresh* `tenant_repo`
/// lookup keyed on `tenant_id`, never from the caller's `database_name`
/// argument. The two are independent on purpose: `database_name` only says
/// what to connect to, `tenant_id` only says what the caller believes it is
/// asking for, and the assertion catches the case where those disagree
/// (DESIGN.md §2.1 — "`expected_database` must trace back to the caller's
/// own intent... resolved separately from whatever the pool-construction
/// code did with it").
pub async fn connect_tenant_pool(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_id: Uuid,
    database_name: &str,
    max_connections: u32,
) -> Result<TenantPool, sqlx::Error> {
    let options = db::with_database_name(base_db_url, database_name)?;

    let tenant = tenant_repo::find_by_id(control_pool, tenant_id)
        .await?
        .ok_or_else(|| {
            sqlx::Error::Configuration(
                format!("no tenant registered with id {tenant_id}").into(),
            )
        })?;

    let pool = db::connect_with_expected_database(
        options,
        max_connections,
        &tenant.database_name,
    )
    .await?;

    Ok(TenantPool { tenant_id, pool })
}

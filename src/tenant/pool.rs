use sqlx::PgPool;

use crate::db;
use crate::profile::Profile;

/// Connects a pool scoped to one tenant's database, derived from
/// `base_db_url` (any URL pointing at the same Postgres cluster — host,
/// port, and credentials are shared across tenants; only the database name
/// varies) by swapping in `database_name`. Wired with the §2.1
/// `current_database()` assertion via `db::connect_with_expected_database`.
pub async fn connect_tenant_pool(
    base_db_url: &str,
    database_name: &str,
    max_connections: u32,
    profile: Profile,
) -> Result<PgPool, sqlx::Error> {
    let options = db::with_database_name(base_db_url, database_name)?;
    db::connect_with_expected_database(options, max_connections, database_name, profile)
        .await
}

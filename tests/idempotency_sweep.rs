//! `idempotency_sweep` integration suite (DESIGN.md §4.3, T-029), following
//! `tests/partition_lifecycle.rs`'s conventions: real provisioning against
//! the local stack, no mocks. `idempotency` has no foreign-key constraints,
//! so rows are inserted directly with fabricated `producer_id`/
//! `comms_request_id` UUIDs -- no need to provision a producer.

use chrono::{Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use messgr::db;
use messgr::idempotency_sweep;
use messgr::keystore::VaultKeyStore;
use messgr::profile::Profile;
use messgr::tenant::pool::connect_tenant_pool;
use messgr::tenant::provision::provision_tenant;

fn control_database_url() -> String {
    dotenvy::dotenv().ok();
    std::env::var("CONTROL_DATABASE_URL")
        .expect("CONTROL_DATABASE_URL must be set for tests")
}

fn vault_keystore() -> VaultKeyStore {
    VaultKeyStore::connect(Profile::Dev).expect("connecting to dev-mode Vault failed")
}

fn unique_name(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

async fn drop_test_tenant(control_pool: &PgPool, database_name: &str, slug: &str) {
    let terminate = format!(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{database_name}'"
    );
    if let Err(err) = sqlx::query(&terminate).execute(control_pool).await {
        eprintln!("cleanup: failed to terminate backends on {database_name}: {err}");
    }

    let drop_db = format!("DROP DATABASE IF EXISTS \"{database_name}\"");
    if let Err(err) = sqlx::query(&drop_db).execute(control_pool).await {
        eprintln!("cleanup: failed to drop database {database_name}: {err}");
    }

    if let Err(err) = sqlx::query("DELETE FROM tenant_schema_version WHERE tenant_id = (SELECT id FROM tenant WHERE slug = $1)")
        .bind(slug)
        .execute(control_pool)
        .await
    {
        eprintln!("cleanup: failed to delete tenant_schema_version for {slug}: {err}");
    }

    if let Err(err) = sqlx::query("DELETE FROM platform_audit WHERE tenant_id = (SELECT id FROM tenant WHERE slug = $1)")
        .bind(slug)
        .execute(control_pool)
        .await
    {
        eprintln!("cleanup: failed to delete platform_audit rows for {slug}: {err}");
    }

    if let Err(err) = sqlx::query("DELETE FROM tenant WHERE slug = $1")
        .bind(slug)
        .execute(control_pool)
        .await
    {
        eprintln!("cleanup: failed to delete tenant row for {slug}: {err}");
    }
}

async fn provision_test_tenant(
    control_pool: &PgPool,
    control_url: &str,
    vault: &VaultKeyStore,
    slug: &str,
    database_name: &str,
) -> Uuid {
    provision_tenant(
        control_pool,
        control_url,
        slug,
        "eu",
        database_name,
        "test-actor",
        vault.client(),
    )
    .await
    .expect("provisioning test tenant failed")
    .tenant_id
}

async fn insert_idempotency_row(pool: &PgPool, expires_at: chrono::DateTime<Utc>) {
    sqlx::query(
        "INSERT INTO idempotency (producer_id, key, comms_request_id, expires_at) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4().to_string())
    .bind(Uuid::new_v4())
    .bind(expires_at)
    .execute(pool)
    .await
    .expect("inserting synthetic idempotency row failed");
}

#[tokio::test]
async fn sweep_deletes_only_rows_past_their_expiry() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_idem_sweep");
    let db_name = unique_name("test_idem_sweep_db");
    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name)
            .await;

    let tenant_pool =
        connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 5)
            .await
            .expect("connecting tenant pool failed")
            .pool;

    let as_of = Utc::now();
    insert_idempotency_row(&tenant_pool, as_of - Duration::days(1)).await;
    insert_idempotency_row(&tenant_pool, as_of + Duration::days(1)).await;

    let deleted =
        idempotency_sweep::run_for_tenant(&control_pool, &control_url, &slug, as_of, 5)
            .await
            .expect("sweep run failed");

    assert_eq!(deleted, 1);

    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM idempotency")
        .fetch_one(&tenant_pool)
        .await
        .expect("counting remaining idempotency rows failed");
    assert_eq!(remaining, 1);

    let remaining_expires_at: chrono::DateTime<Utc> =
        sqlx::query_scalar("SELECT expires_at FROM idempotency")
            .fetch_one(&tenant_pool)
            .await
            .expect("fetching remaining row's expires_at failed");
    assert!(remaining_expires_at > as_of);

    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

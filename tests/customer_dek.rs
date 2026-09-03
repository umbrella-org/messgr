//! `customer_dek` integration suite (DESIGN.md §4.5, §7.6, T-008), following
//! `tests/tenant_config.rs`'s conventions: real provisioning against the
//! local stack, no mocks.

use std::num::NonZeroUsize;
use std::time::Duration;

use sqlx::PgPool;
use uuid::Uuid;

use messgr::customer_dek::lifecycle::{
    get_or_create_dek, pre_provision_deks, pre_provision_for_tenant,
};
use messgr::customer_dek::repo;
use messgr::db;
use messgr::key_cache::KeyCache;
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

    if let Err(err) = sqlx::query(
        "DELETE FROM tenant_schema_version WHERE tenant_id = (SELECT id FROM tenant WHERE slug = $1)",
    )
    .bind(slug)
    .execute(control_pool)
    .await
    {
        eprintln!("cleanup: failed to delete tenant_schema_version for {slug}: {err}");
    }

    if let Err(err) = sqlx::query(
        "DELETE FROM platform_audit WHERE tenant_id = (SELECT id FROM tenant WHERE slug = $1)",
    )
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

fn small_cache() -> KeyCache {
    KeyCache::new(NonZeroUsize::new(16).unwrap(), Duration::from_secs(60))
}

#[tokio::test]
async fn pre_provision_deks_creates_rows_and_is_idempotent() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_customer_dek_preprov");
    let db_name = unique_name("test_db_customer_dek_preprov");
    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name)
            .await;

    let tenant_pool =
        connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 5)
            .await
            .expect("connecting tenant pool failed")
            .pool;
    let mount = format!("transit/{slug}");
    let customer_ids: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();

    let first = pre_provision_deks(&tenant_pool, &vault, &mount, &customer_ids)
        .await
        .expect("first pre-provisioning run failed");
    assert_eq!(first.created, 3);
    assert_eq!(first.already_existed, 0);

    let mut wrapped_before = Vec::with_capacity(customer_ids.len());
    for &id in &customer_ids {
        let row = repo::find(&tenant_pool, id)
            .await
            .expect("find failed")
            .expect("row must exist");
        wrapped_before.push(row.wrapped_dek);
    }

    let second = pre_provision_deks(&tenant_pool, &vault, &mount, &customer_ids)
        .await
        .expect("second pre-provisioning run failed");
    assert_eq!(second.created, 0);
    assert_eq!(second.already_existed, 3);

    for (id, wrapped) in customer_ids.iter().zip(wrapped_before.iter()) {
        let row = repo::find(&tenant_pool, *id)
            .await
            .expect("find failed")
            .expect("row must still exist");
        assert_eq!(
            &row.wrapped_dek, wrapped,
            "re-running pre-provisioning must not mint a new DEK for an existing customer"
        );
    }

    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn get_or_create_dek_creates_one_lazily_on_first_call() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_customer_dek_lazy");
    let db_name = unique_name("test_db_customer_dek_lazy");
    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name)
            .await;

    let tenant_pool =
        connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 5)
            .await
            .expect("connecting tenant pool failed")
            .pool;
    let mount = format!("transit/{slug}");
    let cache = small_cache();
    let customer_id = Uuid::new_v4();

    assert!(
        repo::find(&tenant_pool, customer_id)
            .await
            .expect("find failed")
            .is_none(),
        "no row must exist before the first call"
    );

    let plaintext =
        get_or_create_dek(&tenant_pool, &vault, &cache, &mount, customer_id)
            .await
            .expect("get_or_create_dek failed");
    assert_eq!(plaintext.len(), 32);

    assert!(
        repo::find(&tenant_pool, customer_id)
            .await
            .expect("find failed")
            .is_some(),
        "a row must exist after the first call"
    );

    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn get_or_create_dek_second_call_is_served_from_the_cache_not_vault() {
    // Mutation-tested (same standard as T-001/F13, T-003/F1, T-004's review):
    // if the second call quietly still needed a reachable Vault mount, this
    // test would fail once the mount is disabled below — proving the cache
    // is genuinely what serves the repeat call, not an accident of timing.
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_customer_dek_cache_hit");
    let db_name = unique_name("test_db_customer_dek_cache_hit");
    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name)
            .await;

    let tenant_pool =
        connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 5)
            .await
            .expect("connecting tenant pool failed")
            .pool;
    let mount = format!("transit/{slug}");
    let cache = small_cache();

    let cached_customer = Uuid::new_v4();
    let first =
        get_or_create_dek(&tenant_pool, &vault, &cache, &mount, cached_customer)
            .await
            .expect("warming the cache failed");

    vaultrs::sys::mount::disable(vault.client(), &mount)
        .await
        .expect("disabling the tenant's Transit mount failed");

    let second = get_or_create_dek(&tenant_pool, &vault, &cache, &mount, cached_customer)
        .await
        .expect(
            "a cache hit must succeed even though the tenant's Transit mount is now disabled",
        );
    assert_eq!(
        *first, *second,
        "the cached call must return the identical plaintext"
    );

    // Negative control: an *uncached* customer against the same disabled
    // mount must fail — proving the positive result above isn't an artifact
    // of the mount-disable itself having silently failed.
    let uncached_customer = Uuid::new_v4();
    let result =
        get_or_create_dek(&tenant_pool, &vault, &cache, &mount, uncached_customer)
            .await;
    assert!(
        result.is_err(),
        "an uncached customer must fail once the tenant's Transit mount is disabled"
    );

    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn pre_provision_for_tenant_against_an_unknown_slug_is_rejected_without_auditing()
{
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let unknown_slug = unique_name("test_customer_dek_unknown_slug");
    let unique_actor = unique_name("test-actor-unknown-slug");

    let result = pre_provision_for_tenant(
        &control_pool,
        &control_url,
        &unknown_slug,
        &vault,
        &[Uuid::new_v4()],
        &unique_actor,
    )
    .await;
    assert!(
        result.is_err(),
        "pre-provisioning against an unknown tenant slug must be rejected"
    );

    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT action FROM platform_audit WHERE actor = $1")
            .bind(&unique_actor)
            .fetch_all(&control_pool)
            .await
            .expect("querying platform_audit failed");
    assert!(
        rows.is_empty(),
        "an unknown tenant slug must fail before any platform_audit row is written \
         (there is no tenant_id to attach one to)"
    );
}

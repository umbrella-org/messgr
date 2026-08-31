//! `tenant_pepper` integration suite (DESIGN.md §7.6, T-008), following
//! `tests/tenant_config.rs`'s conventions: real provisioning against the
//! local stack, no mocks.

use sqlx::PgPool;
use uuid::Uuid;

use messgr::db;
use messgr::destination_hmac;
use messgr::keystore::VaultKeyStore;
use messgr::profile::Profile;
use messgr::tenant::provision::provision_tenant;
use messgr::tenant::repo as tenant_repo;
use messgr::tenant_pepper::ensure_tenant_pepper;

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
) {
    provision_tenant(
        control_pool,
        control_url,
        slug,
        "eu",
        database_name,
        Profile::Dev,
        "test-actor",
        vault.client(),
    )
    .await
    .expect("provisioning test tenant failed");
}

#[tokio::test]
async fn ensure_tenant_pepper_mints_and_persists_on_first_call() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_tenant_pepper_mint");
    let db_name = unique_name("test_db_tenant_pepper_mint");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    let tenant = tenant_repo::find_by_slug(&control_pool, &slug)
        .await
        .expect("find_by_slug failed")
        .expect("tenant must exist");
    assert!(
        tenant.vault_pepper_wrapped.is_none(),
        "a freshly provisioned tenant must have no pepper yet"
    );

    let plaintext = ensure_tenant_pepper(&control_pool, &vault, &tenant)
        .await
        .expect("minting the pepper failed");
    assert_eq!(plaintext.len(), 32);

    let reloaded = tenant_repo::find_by_slug(&control_pool, &slug)
        .await
        .expect("find_by_slug failed")
        .expect("tenant must exist");
    assert!(
        reloaded.vault_pepper_wrapped.is_some(),
        "the wrapped pepper must be persisted after the first call"
    );

    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn ensure_tenant_pepper_second_call_reuses_the_persisted_pepper() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_tenant_pepper_reuse");
    let db_name = unique_name("test_db_tenant_pepper_reuse");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    let tenant = tenant_repo::find_by_slug(&control_pool, &slug)
        .await
        .expect("find_by_slug failed")
        .expect("tenant must exist");

    let first = ensure_tenant_pepper(&control_pool, &vault, &tenant)
        .await
        .expect("first mint failed");
    let wrapped_after_first = tenant_repo::find_by_slug(&control_pool, &slug)
        .await
        .expect("find_by_slug failed")
        .expect("tenant must exist")
        .vault_pepper_wrapped
        .expect("wrapped pepper must be persisted");

    // Re-fetch the tenant row (now carrying the persisted wrapped pepper)
    // before the second call, exactly as a real caller would.
    let tenant_with_pepper = tenant_repo::find_by_slug(&control_pool, &slug)
        .await
        .expect("find_by_slug failed")
        .expect("tenant must exist");
    let second = ensure_tenant_pepper(&control_pool, &vault, &tenant_with_pepper)
        .await
        .expect("second call failed");

    assert_eq!(
        *first, *second,
        "the second call must return byte-identical plaintext"
    );

    let wrapped_after_second = tenant_repo::find_by_slug(&control_pool, &slug)
        .await
        .expect("find_by_slug failed")
        .expect("tenant must exist")
        .vault_pepper_wrapped
        .expect("wrapped pepper must still be persisted");
    assert_eq!(
        wrapped_after_first, wrapped_after_second,
        "the second call must not mint a new pepper"
    );

    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn a_real_pepper_produces_distinct_hmacs_for_distinct_destinations() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_tenant_pepper_hmac");
    let db_name = unique_name("test_db_tenant_pepper_hmac");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    let tenant = tenant_repo::find_by_slug(&control_pool, &slug)
        .await
        .expect("find_by_slug failed")
        .expect("tenant must exist");
    let pepper = ensure_tenant_pepper(&control_pool, &vault, &tenant)
        .await
        .expect("minting the pepper failed");

    let hmac_a = destination_hmac::compute(&pepper, "+15550100");
    let hmac_b = destination_hmac::compute(&pepper, "+15550199");
    assert_ne!(hmac_a, hmac_b);

    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

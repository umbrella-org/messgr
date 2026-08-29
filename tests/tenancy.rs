//! DESIGN.md §14: "From step 0b onward, CI runs a two-tenant integration
//! suite" — isolation must be tested, not asserted. This is that suite's
//! assertions:
//!
//! 1. Two provisioned tenants each get a pool whose `current_database()`
//!    matches their own `database_name` — they are not accidentally sharing
//!    one database or one pool.
//! 2. Work performed against tenant A's pool is not visible from tenant B's
//!    pool — a real write, not just a `current_database()` comparison.
//! 3. A pool deliberately mis-wired to the wrong expected tenant trips the
//!    `current_database()` assertion (§2.1) instead of silently connecting.
//!    (The other half of §2.1's assertion — the same check firing again on
//!    every checkout via `before_acquire` — is a unit test next to the
//!    assertion itself in `src/db.rs`, since it needs access to the private
//!    `assert_current_database` helper.)

use sqlx::{Executor, PgPool};
use uuid::Uuid;

use messgr::db;
use messgr::keystore::VaultKeyStore;
use messgr::profile::Profile;
use messgr::tenant::provision::provision_tenant;

fn control_database_url() -> String {
    dotenvy::dotenv().ok();
    std::env::var("CONTROL_DATABASE_URL")
        .expect("CONTROL_DATABASE_URL must be set for tests")
}

/// Every `provision_tenant` call needs an admin Vault client (T-004). One
/// connected `VaultKeyStore` per test, its `.client()` passed to each call.
fn vault_keystore() -> VaultKeyStore {
    VaultKeyStore::connect(Profile::Dev).expect("connecting to dev-mode Vault failed")
}

fn unique_name(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

async fn drop_test_tenant(control_pool: &PgPool, database_name: &str, slug: &str) {
    // Best-effort: a cleanup failure must not fail the test that already
    // made its assertions, and must not stop the rest of cleanup running.
    let terminate = format!(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{database_name}'"
    );
    if let Err(err) = control_pool.execute(terminate.as_str()).await {
        eprintln!("cleanup: failed to terminate backends on {database_name}: {err}");
    }

    let drop_db = format!("DROP DATABASE IF EXISTS \"{database_name}\"");
    if let Err(err) = control_pool.execute(drop_db.as_str()).await {
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

#[tokio::test]
async fn two_tenants_are_isolated_by_database() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug_a = unique_name("test_tenant_a");
    let db_a = unique_name("test_db_a");
    let slug_b = unique_name("test_tenant_b");
    let db_b = unique_name("test_db_b");

    let tenant_a = provision_tenant(
        &control_pool,
        &control_url,
        &slug_a,
        "eu",
        &db_a,
        Profile::Dev,
        "test-actor",
        vault.client(),
    )
    .await
    .expect("provisioning tenant A failed");
    let tenant_b = provision_tenant(
        &control_pool,
        &control_url,
        &slug_b,
        "eu",
        &db_b,
        Profile::Dev,
        "test-actor",
        vault.client(),
    )
    .await
    .expect("provisioning tenant B failed");
    assert_ne!(tenant_a.tenant_id, tenant_b.tenant_id);

    let pool_a =
        messgr::tenant::pool::connect_tenant_pool(&control_url, &db_a, 2, Profile::Dev)
            .await
            .expect("connecting tenant A's pool failed");
    let pool_b =
        messgr::tenant::pool::connect_tenant_pool(&control_url, &db_b, 2, Profile::Dev)
            .await
            .expect("connecting tenant B's pool failed");

    let current_a: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool_a)
        .await
        .unwrap();
    let current_b: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool_b)
        .await
        .unwrap();

    assert_eq!(
        current_a, db_a,
        "tenant A's pool is not connected to its own database"
    );
    assert_eq!(
        current_b, db_b,
        "tenant B's pool is not connected to its own database"
    );
    assert_ne!(
        current_a, current_b,
        "the two tenant pools must not resolve to the same database"
    );

    pool_a.close().await;
    pool_b.close().await;

    drop_test_tenant(&control_pool, &db_a, &slug_a).await;
    drop_test_tenant(&control_pool, &db_b, &slug_b).await;
}

#[tokio::test]
async fn work_on_tenant_as_pool_never_reads_or_writes_tenant_bs_database() {
    // DESIGN.md §14: "Work performed in tenant A's context never reads or
    // writes a row in tenant B's database." The suite's other test only
    // proves the two pools report different `current_database()` names
    // (review finding T-001/F3); this proves the property `current_database`
    // is a proxy for — an actual write on A is actually invisible to B. No
    // ledger table exists yet (a later ticket), so this uses a scratch table
    // created only on A's pool.
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug_a = unique_name("test_tenant_write_a");
    let db_a = unique_name("test_db_write_a");
    let slug_b = unique_name("test_tenant_write_b");
    let db_b = unique_name("test_db_write_b");

    provision_tenant(
        &control_pool,
        &control_url,
        &slug_a,
        "eu",
        &db_a,
        Profile::Dev,
        "test-actor",
        vault.client(),
    )
    .await
    .expect("provisioning tenant A failed");
    provision_tenant(
        &control_pool,
        &control_url,
        &slug_b,
        "eu",
        &db_b,
        Profile::Dev,
        "test-actor",
        vault.client(),
    )
    .await
    .expect("provisioning tenant B failed");

    let pool_a =
        messgr::tenant::pool::connect_tenant_pool(&control_url, &db_a, 2, Profile::Dev)
            .await
            .expect("connecting tenant A's pool failed");
    let pool_b =
        messgr::tenant::pool::connect_tenant_pool(&control_url, &db_b, 2, Profile::Dev)
            .await
            .expect("connecting tenant B's pool failed");

    pool_a
        .execute("CREATE TABLE isolation_probe (id int)")
        .await
        .expect("creating the scratch table on tenant A failed");
    pool_a
        .execute("INSERT INTO isolation_probe (id) VALUES (1)")
        .await
        .expect("inserting into tenant A's scratch table failed");

    let visible_on_b: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('isolation_probe')::text")
            .fetch_one(&pool_b)
            .await
            .expect("checking tenant B for the scratch table failed");

    assert_eq!(
        visible_on_b, None,
        "a table written on tenant A's pool must not be visible from tenant B's pool"
    );

    pool_a.close().await;
    pool_b.close().await;

    drop_test_tenant(&control_pool, &db_a, &slug_a).await;
    drop_test_tenant(&control_pool, &db_b, &slug_b).await;
}

#[tokio::test]
async fn a_mis_wired_pool_trips_the_current_database_assertion() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_tenant_mis_wired");
    let db_name = unique_name("test_db_mis_wired");

    provision_tenant(
        &control_pool,
        &control_url,
        &slug,
        "eu",
        &db_name,
        Profile::Dev,
        "test-actor",
        vault.client(),
    )
    .await
    .expect("provisioning failed");

    // Connect to the real tenant database, but tell the assertion to expect
    // a different name. This is the exact mis-wiring §2.1's assertion exists
    // to catch: code reaching for the wrong tenant's identity.
    let options = db::with_database_name(&control_url, &db_name)
        .expect("parsing the control URL failed");
    let wrong_expected = "not_the_real_database";

    let result = tokio::spawn(async move {
        db::connect_with_expected_database(options, 2, wrong_expected, Profile::Dev)
            .await
    })
    .await;

    assert!(
        result.is_err(),
        "connecting with a deliberately wrong expected database must panic, not succeed"
    );

    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn provisioning_writes_a_platform_audit_row() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_tenant_audit_created");
    let db_name = unique_name("test_db_audit_created");

    let outcome = provision_tenant(
        &control_pool,
        &control_url,
        &slug,
        "eu",
        &db_name,
        Profile::Dev,
        "test-actor",
        vault.client(),
    )
    .await
    .expect("provisioning failed");

    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT action, detail->>'outcome' FROM platform_audit WHERE tenant_id = $1 ORDER BY at",
    )
    .bind(outcome.tenant_id)
    .fetch_all(&control_pool)
    .await
    .expect("querying platform_audit failed");

    assert_eq!(
        rows,
        vec![("tenant.provision".to_string(), Some("created".to_string()))],
        "provisioning a new tenant must write exactly one tenant.provision/created row"
    );

    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn idempotent_reprovision_writes_a_second_platform_audit_row() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_tenant_audit_idempotent");
    let db_name = unique_name("test_db_audit_idempotent");

    let first = provision_tenant(
        &control_pool,
        &control_url,
        &slug,
        "eu",
        &db_name,
        Profile::Dev,
        "test-actor",
        vault.client(),
    )
    .await
    .expect("first provisioning failed");

    provision_tenant(
        &control_pool,
        &control_url,
        &slug,
        "eu",
        &db_name,
        Profile::Dev,
        "test-actor",
        vault.client(),
    )
    .await
    .expect("idempotent re-provisioning failed");

    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT action, detail->>'outcome' FROM platform_audit WHERE tenant_id = $1 ORDER BY at",
    )
    .bind(first.tenant_id)
    .fetch_all(&control_pool)
    .await
    .expect("querying platform_audit failed");

    assert_eq!(
        rows,
        vec![
            ("tenant.provision".to_string(), Some("created".to_string())),
            (
                "tenant.provision".to_string(),
                Some("idempotent".to_string())
            ),
        ],
        "a repeat provisioning with identical inputs must write a second row with outcome=idempotent"
    );

    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn rejected_reprovision_writes_a_platform_audit_row() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_tenant_audit_rejected");
    let db_name = unique_name("test_db_audit_rejected");
    let conflicting_db_name = unique_name("test_db_audit_rejected_conflict");

    let first = provision_tenant(
        &control_pool,
        &control_url,
        &slug,
        "eu",
        &db_name,
        Profile::Dev,
        "test-actor",
        vault.client(),
    )
    .await
    .expect("first provisioning failed");

    let result = provision_tenant(
        &control_pool,
        &control_url,
        &slug,
        "eu",
        &conflicting_db_name,
        Profile::Dev,
        "test-actor",
        vault.client(),
    )
    .await;

    assert!(
        result.is_err(),
        "re-provisioning the same slug with a different database_name must be rejected"
    );

    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT action, detail->>'attempted_database_name' FROM platform_audit \
         WHERE tenant_id = $1 AND action = 'tenant.provision_rejected'",
    )
    .bind(first.tenant_id)
    .fetch_all(&control_pool)
    .await
    .expect("querying platform_audit failed");

    assert_eq!(
        rows,
        vec![(
            "tenant.provision_rejected".to_string(),
            Some(conflicting_db_name.clone())
        )],
        "a rejected re-provision must write exactly one tenant.provision_rejected row naming the attempted database_name"
    );

    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

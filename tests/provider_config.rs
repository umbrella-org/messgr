//! `provider_config` integration suite (DESIGN.md §4.10, §12.1, T-012),
//! following `tests/tenant_config.rs`'s conventions: real provisioning
//! against the local stack, no mocks.

use sqlx::PgPool;
use uuid::Uuid;

use messgr::db;
use messgr::keystore::VaultKeyStore;
use messgr::profile::Profile;
use messgr::provider_config::configure::set_provider_config;
use messgr::provider_config::model::ProviderConfigInput;
use messgr::provider_config::repo;
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

fn sample_input(priority: i16, rate_limit_per_sec: i32) -> ProviderConfigInput {
    ProviderConfigInput {
        channel: "sms".to_string(),
        priority,
        provider: "generic-http".to_string(),
        credential_path: "secret/data/acme/sms".to_string(),
        rate_limit_per_sec,
    }
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

#[tokio::test]
async fn list_returns_empty_before_any_config_is_set() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_provider_config_empty");
    let db_name = unique_name("test_db_pc_empty");
    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    let tenant_pool = connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 2)
        .await
        .expect("connecting tenant pool failed")
        .pool;

    let rows = repo::list(&tenant_pool, "sms")
        .await
        .expect("listing provider_config failed");
    assert!(
        rows.is_empty(),
        "a freshly provisioned tenant must have no provider_config rows"
    );

    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn setting_and_listing_round_trips_every_typed_field() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_provider_config_roundtrip");
    let db_name = unique_name("test_db_pc_roundtrip");
    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    let outcome = set_provider_config(
        &control_pool,
        &control_url,
        &slug,
        sample_input(1, 10),
        "test-actor",
    )
    .await
    .expect("setting provider_config failed");
    assert_eq!(outcome.outcome, "created");

    let tenant_pool = connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 2)
        .await
        .expect("connecting tenant pool failed")
        .pool;
    let rows = repo::list(&tenant_pool, "sms")
        .await
        .expect("listing provider_config failed");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].channel, "sms");
    assert_eq!(rows[0].priority, 1);
    assert_eq!(rows[0].provider, "generic-http");
    assert_eq!(rows[0].credential_path, "secret/data/acme/sms");
    assert_eq!(rows[0].rate_limit_per_sec, 10);

    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn a_second_priority_extends_the_list_in_priority_order() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_provider_config_ordered");
    let db_name = unique_name("test_db_pc_ordered");
    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    set_provider_config(
        &control_pool,
        &control_url,
        &slug,
        sample_input(2, 20),
        "test-actor",
    )
    .await
    .expect("setting priority 2 failed");
    set_provider_config(
        &control_pool,
        &control_url,
        &slug,
        sample_input(1, 10),
        "test-actor",
    )
    .await
    .expect("setting priority 1 failed");

    let tenant_pool = connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 2)
        .await
        .expect("connecting tenant pool failed")
        .pool;
    let rows = repo::list(&tenant_pool, "sms")
        .await
        .expect("listing provider_config failed");

    assert_eq!(
        rows.len(),
        2,
        "both rows must extend the same channel's list"
    );
    assert_eq!(rows[0].priority, 1, "list must be ordered by priority");
    assert_eq!(rows[1].priority, 2);

    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn resetting_with_identical_inputs_is_idempotent_and_does_not_duplicate_the_row()
{
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_provider_config_idempotent");
    let db_name = unique_name("test_db_pc_idempotent");
    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    let first = set_provider_config(
        &control_pool,
        &control_url,
        &slug,
        sample_input(1, 10),
        "test-actor",
    )
    .await
    .expect("first set failed");
    assert_eq!(first.outcome, "created");

    let second = set_provider_config(
        &control_pool,
        &control_url,
        &slug,
        sample_input(1, 10),
        "test-actor",
    )
    .await
    .expect("second set failed");
    assert_eq!(second.outcome, "idempotent");

    let tenant_pool = connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 2)
        .await
        .expect("connecting tenant pool failed")
        .pool;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM provider_config")
        .fetch_one(&tenant_pool)
        .await
        .expect("counting provider_config rows failed");
    assert_eq!(count, 1, "no duplicate provider_config row must be created");

    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn resetting_with_different_inputs_updates_the_row() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_provider_config_update");
    let db_name = unique_name("test_db_pc_update");
    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    set_provider_config(
        &control_pool,
        &control_url,
        &slug,
        sample_input(1, 10),
        "test-actor",
    )
    .await
    .expect("first set failed");

    let second = set_provider_config(
        &control_pool,
        &control_url,
        &slug,
        sample_input(1, 20),
        "test-actor",
    )
    .await
    .expect("second set failed");
    assert_eq!(second.outcome, "updated");

    let tenant_pool = connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 2)
        .await
        .expect("connecting tenant pool failed")
        .pool;
    let rows = repo::list(&tenant_pool, "sms")
        .await
        .expect("listing provider_config failed");
    assert_eq!(rows.len(), 1, "an update must not create a second row");
    assert_eq!(rows[0].rate_limit_per_sec, 20);

    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn set_provider_config_against_an_unknown_tenant_slug_is_rejected_and_audited() {
    // Guards against the T-005/F1 mistake: an unknown tenant slug must not
    // skip the platform_audit write just because it returns early.
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");

    let unknown_slug = unique_name("test_provider_config_unknown_slug");
    let unique_actor = unique_name("test-actor-unknown-slug");

    let result = set_provider_config(
        &control_pool,
        &control_url,
        &unknown_slug,
        sample_input(1, 10),
        &unique_actor,
    )
    .await;
    assert!(
        result.is_err(),
        "setting provider_config against an unknown tenant slug must be rejected"
    );

    let rows: Vec<(String, Option<String>, bool)> = sqlx::query_as(
        "SELECT action, detail->>'outcome', tenant_id IS NULL FROM platform_audit \
         WHERE actor = $1 ORDER BY at",
    )
    .bind(&unique_actor)
    .fetch_all(&control_pool)
    .await
    .expect("querying platform_audit failed");

    assert_eq!(
        rows,
        vec![(
            "provider_config.set".to_string(),
            Some("rejected".to_string()),
            true
        )],
        "a rejected attempt against an unknown tenant slug must write a rejected \
         platform_audit row with no tenant_id, not skip auditing entirely"
    );

    if let Err(err) = sqlx::query("DELETE FROM platform_audit WHERE actor = $1")
        .bind(&unique_actor)
        .execute(&control_pool)
        .await
    {
        eprintln!(
            "cleanup: failed to delete platform_audit rows for actor {unique_actor}: {err}"
        );
    }
}

//! `tenant_config` integration suite (DESIGN.md §4.10, T-007), following
//! `tests/producer.rs`'s conventions: real provisioning against the local
//! stack, no mocks.

use sqlx::PgPool;
use uuid::Uuid;

use messgr::db;
use messgr::keystore::VaultKeyStore;
use messgr::profile::Profile;
use messgr::tenant::pool::connect_tenant_pool;
use messgr::tenant::provision::provision_tenant;
use messgr::tenant_config::configure::set_tenant_config;
use messgr::tenant_config::model::{TenantConfigInput, verification_mode};
use messgr::tenant_config::repo;

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

fn sample_input(retention_years: i32) -> TenantConfigInput {
    TenantConfigInput {
        retention_years,
        default_timezone: "Europe/London".to_string(),
        default_locale: "en-GB".to_string(),
        schedule_horizon_days: 90,
        quota_day_boundary_tz: "Europe/London".to_string(),
        verification_mode: verification_mode::OBSERVE.to_string(),
        kill_switch_release_rate: 500,
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
async fn load_returns_none_before_any_config_is_set() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_tenant_config_none");
    let db_name = unique_name("test_db_config_none");
    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name)
            .await;

    let tenant_pool =
        connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 2)
            .await
            .expect("connecting tenant pool failed")
            .pool;

    let loaded = repo::load(&tenant_pool)
        .await
        .expect("loading tenant_config failed");
    assert!(
        loaded.is_none(),
        "a freshly provisioned tenant must have no tenant_config row"
    );

    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn setting_and_loading_round_trips_every_typed_field() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_tenant_config_roundtrip");
    let db_name = unique_name("test_db_config_roundtrip");
    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name)
            .await;

    let outcome = set_tenant_config(
        &control_pool,
        &control_url,
        &slug,
        sample_input(7),
        "test-actor",
    )
    .await
    .expect("setting tenant_config failed");
    assert_eq!(outcome.outcome, "created");

    let tenant_pool =
        connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 2)
            .await
            .expect("connecting tenant pool failed")
            .pool;
    let loaded = repo::load(&tenant_pool)
        .await
        .expect("loading tenant_config failed")
        .expect("tenant_config row must exist after set_tenant_config");

    assert_eq!(loaded.retention_years, 7);
    assert_eq!(loaded.default_timezone, "Europe/London");
    assert_eq!(loaded.default_locale, "en-GB");
    assert_eq!(loaded.schedule_horizon_days, 90);
    assert_eq!(loaded.quota_day_boundary_tz, "Europe/London");
    assert_eq!(loaded.verification_mode, verification_mode::OBSERVE);
    assert_eq!(loaded.kill_switch_release_rate, 500);

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

    let slug = unique_name("test_tenant_config_idempotent");
    let db_name = unique_name("test_db_config_idempotent");
    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name)
            .await;

    let first = set_tenant_config(
        &control_pool,
        &control_url,
        &slug,
        sample_input(7),
        "test-actor",
    )
    .await
    .expect("first set failed");
    assert_eq!(first.outcome, "created");

    let second = set_tenant_config(
        &control_pool,
        &control_url,
        &slug,
        sample_input(7),
        "test-actor",
    )
    .await
    .expect("second set failed");
    assert_eq!(second.outcome, "idempotent");

    let tenant_pool =
        connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 2)
            .await
            .expect("connecting tenant pool failed")
            .pool;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM tenant_config")
        .fetch_one(&tenant_pool)
        .await
        .expect("counting tenant_config rows failed");
    assert_eq!(count, 1, "no duplicate tenant_config row must be created");

    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn concurrent_first_time_set_calls_with_identical_input_audit_one_created_and_one_idempotent()
 {
    // T-026: set_tenant_config_inner used to decide its audit outcome from
    // a `load` taken before the race — two concurrent first-time callers
    // could both read no row, both compute "created", and both audit
    // "created" even though only one of them actually created the row.
    // repo::upsert's ON CONFLICT DO UPDATE always made the write itself
    // safe; lock_tx's advisory lock is what makes the classification
    // accurate under a race.
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_tenant_config_concurrent");
    let db_name = unique_name("test_db_config_concurrent");
    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name)
            .await;

    let call_one = set_tenant_config(
        &control_pool,
        &control_url,
        &slug,
        sample_input(7),
        "test-actor",
    );
    let call_two = set_tenant_config(
        &control_pool,
        &control_url,
        &slug,
        sample_input(7),
        "test-actor",
    );
    let (result_one, result_two) = tokio::join!(call_one, call_two);
    let result_one = result_one.expect("first concurrent set failed");
    let result_two = result_two.expect("second concurrent set failed");

    let outcomes = [result_one.outcome, result_two.outcome];
    assert_eq!(
        outcomes.iter().filter(|o| **o == "created").count(),
        1,
        "exactly one concurrent set must report created: {outcomes:?}"
    );
    assert_eq!(
        outcomes.iter().filter(|o| **o == "idempotent").count(),
        1,
        "exactly one concurrent set must report idempotent, never both created: {outcomes:?}"
    );

    let tenant_pool =
        connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 2)
            .await
            .expect("connecting tenant pool failed")
            .pool;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM tenant_config")
        .fetch_one(&tenant_pool)
        .await
        .expect("counting tenant_config rows failed");
    assert_eq!(count, 1, "no duplicate tenant_config row must be created");

    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn resetting_with_different_inputs_updates_the_singleton_row() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_tenant_config_update");
    let db_name = unique_name("test_db_config_update");
    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name)
            .await;

    set_tenant_config(
        &control_pool,
        &control_url,
        &slug,
        sample_input(7),
        "test-actor",
    )
    .await
    .expect("first set failed");

    let second = set_tenant_config(
        &control_pool,
        &control_url,
        &slug,
        sample_input(3),
        "test-actor",
    )
    .await
    .expect("second set failed");
    assert_eq!(second.outcome, "updated");

    let tenant_pool =
        connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 2)
            .await
            .expect("connecting tenant pool failed")
            .pool;
    let loaded = repo::load(&tenant_pool)
        .await
        .expect("loading tenant_config failed")
        .expect("tenant_config row must exist");
    assert_eq!(loaded.retention_years, 3);

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM tenant_config")
        .fetch_one(&tenant_pool)
        .await
        .expect("counting tenant_config rows failed");
    assert_eq!(count, 1, "an update must not create a second row");

    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn set_tenant_config_against_an_unknown_tenant_slug_is_rejected_and_audited() {
    // Guards against the T-005/F1 mistake: an unknown tenant slug must not
    // skip the platform_audit write just because it returns early.
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");

    let unknown_slug = unique_name("test_tenant_config_unknown_slug");
    let unique_actor = unique_name("test-actor-unknown-slug");

    let result = set_tenant_config(
        &control_pool,
        &control_url,
        &unknown_slug,
        sample_input(7),
        &unique_actor,
    )
    .await;
    assert!(
        result.is_err(),
        "setting tenant_config against an unknown tenant slug must be rejected"
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
            "tenant_config.set".to_string(),
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

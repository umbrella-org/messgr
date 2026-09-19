//! `quiet_hours` integration suite (DESIGN.md §4.10, §6.1, T-043), following
//! `tests/consent.rs`'s conventions: real provisioning against the local
//! stack, no mocks.

use chrono::NaiveTime;
use sqlx::PgPool;
use uuid::Uuid;

use messgr::db;
use messgr::keystore::VaultKeyStore;
use messgr::profile::Profile;
use messgr::quiet_hours::configure::{set_quiet_hours_policy, show_quiet_hours_policy};
use messgr::quiet_hours::model::QuietHoursPolicyInput;
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

#[tokio::test]
async fn setting_and_reading_back_round_trips_start_and_end() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_quiet_hours_roundtrip");
    let db_name = unique_name("test_db_quiet_hours_roundtrip");
    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name)
            .await;

    let outcome = set_quiet_hours_policy(
        &control_pool,
        &control_url,
        &slug,
        QuietHoursPolicyInput {
            start_local: NaiveTime::from_hms_opt(22, 0, 0).unwrap(),
            end_local: NaiveTime::from_hms_opt(7, 0, 0).unwrap(),
        },
        "test-actor",
    )
    .await
    .expect("setting quiet_hours_policy failed");
    assert_eq!(outcome.outcome, "created");

    let policy = show_quiet_hours_policy(&control_pool, &control_url, &slug)
        .await
        .expect("showing quiet_hours_policy failed")
        .expect("policy must exist after set");
    assert_eq!(
        policy.start_local,
        NaiveTime::from_hms_opt(22, 0, 0).unwrap()
    );
    assert_eq!(policy.end_local, NaiveTime::from_hms_opt(7, 0, 0).unwrap());

    let tenant_pool =
        connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 2)
            .await
            .expect("connecting tenant pool failed")
            .pool;
    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn setting_again_with_a_different_window_updates_the_row_without_duplicating_it()
{
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_quiet_hours_update");
    let db_name = unique_name("test_db_quiet_hours_update");
    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name)
            .await;

    set_quiet_hours_policy(
        &control_pool,
        &control_url,
        &slug,
        QuietHoursPolicyInput {
            start_local: NaiveTime::from_hms_opt(22, 0, 0).unwrap(),
            end_local: NaiveTime::from_hms_opt(7, 0, 0).unwrap(),
        },
        "test-actor",
    )
    .await
    .expect("first set failed");

    let second = set_quiet_hours_policy(
        &control_pool,
        &control_url,
        &slug,
        QuietHoursPolicyInput {
            start_local: NaiveTime::from_hms_opt(21, 0, 0).unwrap(),
            end_local: NaiveTime::from_hms_opt(6, 0, 0).unwrap(),
        },
        "test-actor",
    )
    .await
    .expect("second set failed");
    assert_eq!(second.outcome, "updated");

    let policy = show_quiet_hours_policy(&control_pool, &control_url, &slug)
        .await
        .expect("showing quiet_hours_policy failed")
        .expect("policy must still exist");
    assert_eq!(
        policy.start_local,
        NaiveTime::from_hms_opt(21, 0, 0).unwrap()
    );
    assert_eq!(policy.end_local, NaiveTime::from_hms_opt(6, 0, 0).unwrap());

    let tenant_pool =
        connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 2)
            .await
            .expect("connecting tenant pool failed")
            .pool;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM quiet_hours_policy")
        .fetch_one(&tenant_pool)
        .await
        .expect("counting quiet_hours_policy rows failed");
    assert_eq!(count, 1, "an update must not create a second row");

    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn resetting_with_identical_inputs_is_idempotent() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_quiet_hours_idempotent");
    let db_name = unique_name("test_db_quiet_hours_idempotent");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    let input = QuietHoursPolicyInput {
        start_local: NaiveTime::from_hms_opt(22, 0, 0).unwrap(),
        end_local: NaiveTime::from_hms_opt(7, 0, 0).unwrap(),
    };

    let first = set_quiet_hours_policy(
        &control_pool,
        &control_url,
        &slug,
        QuietHoursPolicyInput {
            start_local: input.start_local,
            end_local: input.end_local,
        },
        "test-actor",
    )
    .await
    .expect("first set failed");
    assert_eq!(first.outcome, "created");

    let second = set_quiet_hours_policy(
        &control_pool,
        &control_url,
        &slug,
        QuietHoursPolicyInput {
            start_local: input.start_local,
            end_local: input.end_local,
        },
        "test-actor",
    )
    .await
    .expect("second set failed");
    assert_eq!(second.outcome, "idempotent");

    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn set_against_an_unknown_tenant_slug_is_rejected_and_audited() {
    // Guards against the T-005/F1 mistake: an unknown tenant slug must not
    // skip the platform_audit write just because it returns early.
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");

    let unknown_slug = unique_name("test_quiet_hours_unknown_slug");
    let unique_actor = unique_name("test-actor-unknown-slug");

    let result = set_quiet_hours_policy(
        &control_pool,
        &control_url,
        &unknown_slug,
        QuietHoursPolicyInput {
            start_local: NaiveTime::from_hms_opt(22, 0, 0).unwrap(),
            end_local: NaiveTime::from_hms_opt(7, 0, 0).unwrap(),
        },
        &unique_actor,
    )
    .await;
    assert!(
        result.is_err(),
        "setting quiet_hours_policy against an unknown tenant slug must be rejected"
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
            "quiet_hours_policy.set".to_string(),
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

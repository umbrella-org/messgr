//! `suppression` integration suite (DESIGN.md §5, §4.4, T-038), following
//! `tests/provider_config.rs`'s conventions: real provisioning against the
//! local stack, no mocks.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use messgr::db;
use messgr::destination_hmac;
use messgr::keystore::VaultKeyStore;
use messgr::profile::Profile;
use messgr::suppression::configure::{add_suppression, remove_suppression};
use messgr::suppression::model::reason;
use messgr::suppression::repo;
use messgr::tenant::pool::connect_tenant_pool;
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

/// Truncated to microsecond precision, matching what a Postgres `timestamptz`
/// round-trip returns -- `add_suppression` truncates internally too, but a
/// value compared against a freshly-inserted row must already match, since
/// nothing here re-reads it through that truncation first.
fn far_future() -> DateTime<Utc> {
    let dt = Utc::now() + chrono::Duration::days(30);
    DateTime::from_timestamp_micros(dt.timestamp_micros()).expect("valid instant")
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

/// Computes the same `destination_hmac` `add_suppression`/`remove_suppression`
/// derive internally, so a test can look a row up by hash without ever
/// storing or passing a precomputed HMAC itself (T-038 decision 6).
async fn destination_hmac_for(
    control_pool: &PgPool,
    vault: &VaultKeyStore,
    slug: &str,
    destination: &str,
) -> Vec<u8> {
    let tenant = tenant_repo::find_by_slug(control_pool, slug)
        .await
        .expect("tenant lookup failed")
        .expect("tenant must exist");
    let pepper = ensure_tenant_pepper(control_pool, vault, &tenant)
        .await
        .expect("resolving tenant pepper failed");
    destination_hmac::compute(&pepper, destination)
}

#[tokio::test]
async fn list_returns_empty_before_any_entry_is_added() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_suppression_empty");
    let db_name = unique_name("test_db_supp_empty");
    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name)
            .await;

    let tenant_pool =
        connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 2)
            .await
            .expect("connecting tenant pool failed")
            .pool;

    let rows = repo::list(&tenant_pool)
        .await
        .expect("listing suppression failed");
    assert!(
        rows.is_empty(),
        "a freshly provisioned tenant must have no suppression rows"
    );

    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn adding_and_listing_round_trips_every_typed_field() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_suppression_roundtrip");
    let db_name = unique_name("test_db_supp_roundtrip");
    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name)
            .await;

    let review_at = far_future();
    let outcome = add_suppression(
        &control_pool,
        &control_url,
        &slug,
        &vault,
        "+15550100",
        reason::HARD_BOUNCE,
        review_at,
        "test-actor",
    )
    .await
    .expect("adding suppression entry failed");
    assert_eq!(outcome.outcome, "created");

    let tenant_pool =
        connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 2)
            .await
            .expect("connecting tenant pool failed")
            .pool;
    let rows = repo::list(&tenant_pool)
        .await
        .expect("listing suppression failed");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].reason, reason::HARD_BOUNCE);
    assert_eq!(rows[0].review_at, review_at);

    let expected_hmac =
        destination_hmac_for(&control_pool, &vault, &slug, "+15550100").await;
    assert_eq!(rows[0].destination_hmac, expected_hmac);

    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn adding_again_with_a_different_reason_updates_the_row_without_duplicating_it() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_suppression_update");
    let db_name = unique_name("test_db_supp_update");
    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name)
            .await;

    add_suppression(
        &control_pool,
        &control_url,
        &slug,
        &vault,
        "+15550100",
        reason::HARD_BOUNCE,
        far_future(),
        "test-actor",
    )
    .await
    .expect("first add failed");

    let second = add_suppression(
        &control_pool,
        &control_url,
        &slug,
        &vault,
        "+15550100",
        reason::COMPLAINT,
        far_future(),
        "test-actor",
    )
    .await
    .expect("second add failed");
    assert_eq!(second.outcome, "updated");

    let tenant_pool =
        connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 2)
            .await
            .expect("connecting tenant pool failed")
            .pool;
    let rows = repo::list(&tenant_pool)
        .await
        .expect("listing suppression failed");
    assert_eq!(rows.len(), 1, "an update must not create a second row");
    assert_eq!(rows[0].reason, reason::COMPLAINT);

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

    let slug = unique_name("test_suppression_idempotent");
    let db_name = unique_name("test_db_supp_idempotent");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    let review_at = far_future();
    let first = add_suppression(
        &control_pool,
        &control_url,
        &slug,
        &vault,
        "+15550100",
        reason::HARD_BOUNCE,
        review_at,
        "test-actor",
    )
    .await
    .expect("first add failed");
    assert_eq!(first.outcome, "created");

    let second = add_suppression(
        &control_pool,
        &control_url,
        &slug,
        &vault,
        "+15550100",
        reason::HARD_BOUNCE,
        review_at,
        "test-actor",
    )
    .await
    .expect("second add failed");
    assert_eq!(second.outcome, "idempotent");

    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn removing_an_active_entry_retires_it() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_suppression_remove");
    let db_name = unique_name("test_db_supp_remove");
    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name)
            .await;

    add_suppression(
        &control_pool,
        &control_url,
        &slug,
        &vault,
        "+15550100",
        reason::HARD_BOUNCE,
        far_future(),
        "test-actor",
    )
    .await
    .expect("add failed");

    remove_suppression(
        &control_pool,
        &control_url,
        &slug,
        &vault,
        "+15550100",
        "test-actor",
    )
    .await
    .expect("remove failed");

    let tenant_pool =
        connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 2)
            .await
            .expect("connecting tenant pool failed")
            .pool;
    let hmac = destination_hmac_for(&control_pool, &vault, &slug, "+15550100").await;
    let row = repo::load_one(&tenant_pool, &hmac)
        .await
        .expect("loading suppression row failed")
        .expect("row must still exist after retirement");
    assert!(
        row.review_at <= Utc::now(),
        "retiring an entry must move review_at to now or earlier"
    );

    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn add_suppression_against_an_unknown_tenant_slug_is_rejected_and_audited() {
    // Guards against the T-005/F1 mistake: an unknown tenant slug must not
    // skip the platform_audit write just because it returns early.
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let unknown_slug = unique_name("test_suppression_unknown_slug");
    let unique_actor = unique_name("test-actor-unknown-slug");

    let result = add_suppression(
        &control_pool,
        &control_url,
        &unknown_slug,
        &vault,
        "+15550100",
        reason::HARD_BOUNCE,
        far_future(),
        &unique_actor,
    )
    .await;
    assert!(
        result.is_err(),
        "adding a suppression entry against an unknown tenant slug must be rejected"
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
            "suppression.add".to_string(),
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

#[tokio::test]
async fn remove_suppression_against_a_destination_with_no_active_entry_is_rejected() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_suppression_remove_none");
    let db_name = unique_name("test_db_supp_remove_none");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    let result = remove_suppression(
        &control_pool,
        &control_url,
        &slug,
        &vault,
        "+15550100",
        "test-actor",
    )
    .await;
    assert!(
        result.is_err(),
        "removing a suppression entry with no active entry must be rejected"
    );

    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

//! `consent` integration suite (DESIGN.md §5, §4.4, T-037), following
//! `tests/suppression.rs`'s conventions: real provisioning against the
//! local stack, no mocks.

use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

use messgr::consent::configure::set_consent;
use messgr::consent::repo;
use messgr::db;
use messgr::destination_hmac;
use messgr::keystore::VaultKeyStore;
use messgr::profile::Profile;
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

/// Inserts a `customer` + active `customer_address` row directly via SQL
/// (`kind = 'msisdn'`, matching `--channel sms`), computing the same
/// `destination_hmac` `set_consent` derives internally (tenant pepper +
/// `destination_hmac::compute`) so `set_consent`'s own HMAC lookup resolves
/// to this exact row. Returns the new `address_id`.
async fn insert_active_address(
    control_pool: &PgPool,
    tenant_pool: &PgPool,
    vault: &VaultKeyStore,
    slug: &str,
    destination: &str,
) -> Uuid {
    let tenant = tenant_repo::find_by_slug(control_pool, slug)
        .await
        .expect("tenant lookup failed")
        .expect("tenant must exist");
    let pepper = ensure_tenant_pepper(control_pool, vault, &tenant)
        .await
        .expect("resolving tenant pepper failed");
    let value_hmac = destination_hmac::compute(&pepper, destination);

    let customer_id = Uuid::new_v4();
    let now = Utc::now();
    sqlx::query(
        "INSERT INTO customer (id, locale, timezone, provisional, created_at) \
         VALUES ($1, 'en-US', 'UTC', false, $2)",
    )
    .bind(customer_id)
    .bind(now)
    .execute(tenant_pool)
    .await
    .expect("inserting customer failed");

    let address_id = Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO customer_address (
            id, customer_id, kind, value_ciphertext, value_hmac, rank, label,
            verified_at, active_from, active_to, source_updated_at
        ) VALUES ($1, $2, 'msisdn', $3, $4, 1, NULL, $5, $5, NULL, $5)
        "#,
    )
    .bind(address_id)
    .bind(customer_id)
    .bind(destination.as_bytes())
    .bind(&value_hmac)
    .bind(now)
    .execute(tenant_pool)
    .await
    .expect("inserting customer_address failed");

    address_id
}

#[tokio::test]
async fn setting_and_reading_back_round_trips_every_typed_field() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_consent_roundtrip");
    let db_name = unique_name("test_db_consent_roundtrip");
    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name)
            .await;

    let tenant_pool =
        connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 2)
            .await
            .expect("connecting tenant pool failed")
            .pool;

    let address_id =
        insert_active_address(&control_pool, &tenant_pool, &vault, &slug, "+15550100")
            .await;

    let outcome = set_consent(
        &control_pool,
        &control_url,
        &slug,
        &vault,
        "+15550100",
        "sms",
        "marketing",
        true,
        "web_form",
        "test-actor",
    )
    .await
    .expect("setting consent failed");
    assert_eq!(outcome.outcome, "created");

    let row = repo::load_one(&tenant_pool, address_id, "marketing")
        .await
        .expect("loading consent row failed")
        .expect("row must exist after set");
    assert_eq!(row.address_id, address_id);
    assert_eq!(row.class, "marketing");
    assert!(row.opted_in);
    assert_eq!(row.source, "web_form");

    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn setting_again_with_a_different_opted_in_value_updates_the_row_without_duplicating_it()
 {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_consent_update");
    let db_name = unique_name("test_db_consent_update");
    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name)
            .await;

    let tenant_pool =
        connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 2)
            .await
            .expect("connecting tenant pool failed")
            .pool;

    let address_id =
        insert_active_address(&control_pool, &tenant_pool, &vault, &slug, "+15550100")
            .await;

    set_consent(
        &control_pool,
        &control_url,
        &slug,
        &vault,
        "+15550100",
        "sms",
        "marketing",
        true,
        "web_form",
        "test-actor",
    )
    .await
    .expect("first set failed");

    let second = set_consent(
        &control_pool,
        &control_url,
        &slug,
        &vault,
        "+15550100",
        "sms",
        "marketing",
        false,
        "sms_stop_reply",
        "test-actor",
    )
    .await
    .expect("second set failed");
    assert_eq!(second.outcome, "updated");

    let row = repo::load_one(&tenant_pool, address_id, "marketing")
        .await
        .expect("loading consent row failed")
        .expect("row must still exist");
    assert!(!row.opted_in);
    assert_eq!(row.source, "sms_stop_reply");

    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM consent WHERE address_id = $1 AND class = 'marketing'",
    )
    .bind(address_id)
    .fetch_one(&tenant_pool)
    .await
    .expect("counting consent rows failed");
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

    let slug = unique_name("test_consent_idempotent");
    let db_name = unique_name("test_db_consent_idempotent");
    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name)
            .await;

    let tenant_pool =
        connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 2)
            .await
            .expect("connecting tenant pool failed")
            .pool;

    insert_active_address(&control_pool, &tenant_pool, &vault, &slug, "+15550100")
        .await;

    let first = set_consent(
        &control_pool,
        &control_url,
        &slug,
        &vault,
        "+15550100",
        "sms",
        "marketing",
        true,
        "web_form",
        "test-actor",
    )
    .await
    .expect("first set failed");
    assert_eq!(first.outcome, "created");

    let second = set_consent(
        &control_pool,
        &control_url,
        &slug,
        &vault,
        "+15550100",
        "sms",
        "marketing",
        true,
        "web_form",
        "test-actor",
    )
    .await
    .expect("second set failed");
    assert_eq!(second.outcome, "idempotent");

    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn consent_set_against_an_unknown_tenant_slug_is_rejected_and_audited() {
    // Guards against the T-005/F1 mistake: an unknown tenant slug must not
    // skip the platform_audit write just because it returns early.
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let unknown_slug = unique_name("test_consent_unknown_slug");
    let unique_actor = unique_name("test-actor-unknown-slug");

    let result = set_consent(
        &control_pool,
        &control_url,
        &unknown_slug,
        &vault,
        "+15550100",
        "sms",
        "marketing",
        true,
        "web_form",
        &unique_actor,
    )
    .await;
    assert!(
        result.is_err(),
        "setting consent against an unknown tenant slug must be rejected"
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
            "consent.set".to_string(),
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
async fn consent_set_against_a_destination_with_no_active_address_is_rejected_and_audited()
 {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_consent_no_address");
    let db_name = unique_name("test_db_consent_no_address");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    let unique_actor = unique_name("test-actor-no-address");
    let result = set_consent(
        &control_pool,
        &control_url,
        &slug,
        &vault,
        "+15550100",
        "sms",
        "marketing",
        true,
        "web_form",
        &unique_actor,
    )
    .await;
    assert!(
        result.is_err(),
        "setting consent against a destination with no active address must be rejected"
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
            "consent.set".to_string(),
            Some("rejected".to_string()),
            false
        )],
        "a rejected attempt against a destination with no active address must carry a real \
         tenant_id, unlike the unknown-tenant-slug case"
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

    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

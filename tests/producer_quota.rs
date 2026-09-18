//! `producer_quota`/`producer_quota_override`/`producer_usage` integration
//! suite (DESIGN.md §5.1, T-042), following `tests/suppression.rs`'s
//! conventions: real provisioning against the local stack, no mocks.

use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

use messgr::db;
use messgr::ingest::model::class;
use messgr::keystore::VaultKeyStore;
use messgr::producer::register::register_producer;
use messgr::producer_quota::configure::{
    add_producer_quota_override, list_producer_quota, list_producer_quota_overrides,
    set_producer_quota,
};
use messgr::producer_quota::model::{UsageRow, enforcement, granularity};
use messgr::producer_quota::tracker::{QuotaDecision, QuotaTracker};
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

async fn register_test_producer(
    control_pool: &PgPool,
    control_url: &str,
    tenant_slug: &str,
    name: &str,
) -> Uuid {
    register_producer(
        control_pool,
        control_url,
        tenant_slug,
        name,
        &format!("CN={name}"),
        "test-team",
        "oncall@example.com",
        "test-actor",
    )
    .await
    .expect("registering producer failed")
    .producer_id
}

#[tokio::test]
async fn setting_and_listing_round_trips_every_typed_field() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_pq_roundtrip");
    let db_name = unique_name("test_db_pq_roundtrip");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    let producer_name = unique_name("producer");
    register_test_producer(&control_pool, &control_url, &slug, &producer_name).await;

    let outcome = set_producer_quota(
        &control_pool,
        &control_url,
        &slug,
        &producer_name,
        "sms",
        class::MARKETING,
        Some(10),
        Some(1000),
        enforcement::HARD,
        "test-actor",
    )
    .await
    .expect("setting producer quota failed");
    assert_eq!(outcome.outcome, "created");

    let rows = list_producer_quota(&control_pool, &control_url, &slug)
        .await
        .expect("listing producer quota failed");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].channel, "sms");
    assert_eq!(rows[0].class, class::MARKETING);
    assert_eq!(rows[0].per_minute, Some(10));
    assert_eq!(rows[0].per_day, Some(1000));
    assert_eq!(rows[0].enforcement, enforcement::HARD);

    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn setting_again_with_different_limits_updates_the_row_without_duplicating_it() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_pq_update");
    let db_name = unique_name("test_db_pq_update");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    let producer_name = unique_name("producer");
    register_test_producer(&control_pool, &control_url, &slug, &producer_name).await;

    set_producer_quota(
        &control_pool,
        &control_url,
        &slug,
        &producer_name,
        "sms",
        class::MARKETING,
        Some(10),
        Some(1000),
        enforcement::HARD,
        "test-actor",
    )
    .await
    .expect("first set failed");

    let second = set_producer_quota(
        &control_pool,
        &control_url,
        &slug,
        &producer_name,
        "sms",
        class::MARKETING,
        Some(20),
        Some(2000),
        enforcement::HARD,
        "test-actor",
    )
    .await
    .expect("second set failed");
    assert_eq!(second.outcome, "updated");

    let rows = list_producer_quota(&control_pool, &control_url, &slug)
        .await
        .expect("listing producer quota failed");
    assert_eq!(rows.len(), 1, "an update must not create a second row");
    assert_eq!(rows[0].per_minute, Some(20));
    assert_eq!(rows[0].per_day, Some(2000));

    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn resetting_with_identical_inputs_is_idempotent() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_pq_idempotent");
    let db_name = unique_name("test_db_pq_idempotent");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    let producer_name = unique_name("producer");
    register_test_producer(&control_pool, &control_url, &slug, &producer_name).await;

    let first = set_producer_quota(
        &control_pool,
        &control_url,
        &slug,
        &producer_name,
        "sms",
        class::MARKETING,
        Some(10),
        Some(1000),
        enforcement::HARD,
        "test-actor",
    )
    .await
    .expect("first set failed");
    assert_eq!(first.outcome, "created");

    let second = set_producer_quota(
        &control_pool,
        &control_url,
        &slug,
        &producer_name,
        "sms",
        class::MARKETING,
        Some(10),
        Some(1000),
        enforcement::HARD,
        "test-actor",
    )
    .await
    .expect("second set failed");
    assert_eq!(second.outcome, "idempotent");

    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn setting_hard_enforcement_for_transactional_class_is_rejected() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_pq_hard_transactional");
    let db_name = unique_name("test_db_pq_hard_transactional");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    let producer_name = unique_name("producer");
    register_test_producer(&control_pool, &control_url, &slug, &producer_name).await;

    let result = set_producer_quota(
        &control_pool,
        &control_url,
        &slug,
        &producer_name,
        "sms",
        class::TRANSACTIONAL,
        Some(10),
        Some(1000),
        enforcement::HARD,
        "test-actor",
    )
    .await;
    assert!(
        result.is_err(),
        "hard enforcement for transactional must be rejected \
         (AGENTS.md invariant 5: quota must never block transactional traffic)"
    );

    let rows = list_producer_quota(&control_pool, &control_url, &slug)
        .await
        .expect("listing producer quota failed");
    assert!(rows.is_empty(), "a rejected set must not write a row");

    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn setting_any_quota_row_for_auth_class_is_rejected() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_pq_auth");
    let db_name = unique_name("test_db_pq_auth");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    let producer_name = unique_name("producer");
    register_test_producer(&control_pool, &control_url, &slug, &producer_name).await;

    let result = set_producer_quota(
        &control_pool,
        &control_url,
        &slug,
        &producer_name,
        "sms",
        class::AUTH,
        Some(10),
        Some(1000),
        enforcement::SOFT,
        "test-actor",
    )
    .await;
    assert!(
        result.is_err(),
        "a quota row for class \"auth\" must be rejected \
         (AGENTS.md invariant 5: quota must never block auth traffic)"
    );

    let rows = list_producer_quota(&control_pool, &control_url, &slug)
        .await
        .expect("listing producer quota failed");
    assert!(rows.is_empty(), "a rejected set must not write a row");

    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn setting_quota_against_an_unknown_producer_name_is_rejected_and_audited() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_pq_unknown_producer");
    let db_name = unique_name("test_db_pq_unknown_producer");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    let unknown_producer = unique_name("no-such-producer");
    let unique_actor = unique_name("test-actor-unknown-producer");

    let result = set_producer_quota(
        &control_pool,
        &control_url,
        &slug,
        &unknown_producer,
        "sms",
        class::MARKETING,
        Some(10),
        Some(1000),
        enforcement::HARD,
        &unique_actor,
    )
    .await;
    assert!(
        result.is_err(),
        "setting a quota against an unknown producer name must be rejected"
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
            "producer_quota.set".to_string(),
            Some("rejected".to_string()),
            false
        )],
        "a rejected attempt against an unknown producer name must still write a \
         platform_audit row (with a real tenant_id, since the tenant itself resolved)"
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

#[tokio::test]
async fn setting_quota_against_an_unknown_tenant_slug_is_rejected_and_audited() {
    // Guards against the T-005/F1 mistake: an unknown tenant slug must not
    // skip the platform_audit write just because it returns early.
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");

    let unknown_slug = unique_name("test_pq_unknown_slug");
    let unique_actor = unique_name("test-actor-unknown-slug");

    let result = set_producer_quota(
        &control_pool,
        &control_url,
        &unknown_slug,
        "does-not-matter",
        "sms",
        class::MARKETING,
        Some(10),
        Some(1000),
        enforcement::HARD,
        &unique_actor,
    )
    .await;
    assert!(
        result.is_err(),
        "setting a quota against an unknown tenant slug must be rejected"
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
            "producer_quota.set".to_string(),
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
async fn adding_an_override_with_valid_to_before_valid_from_is_rejected() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_pq_override_bad_range");
    let db_name = unique_name("test_db_pq_override_bad_range");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    let producer_name = unique_name("producer");
    register_test_producer(&control_pool, &control_url, &slug, &producer_name).await;

    let now = Utc::now();
    let result = add_producer_quota_override(
        &control_pool,
        &control_url,
        &slug,
        &producer_name,
        "sms",
        class::MARKETING,
        500,
        now,
        now - chrono::Duration::days(1),
        "ops-lead",
        "campaign day uplift",
        "test-actor",
    )
    .await;
    assert!(
        result.is_err(),
        "an override with valid_to before valid_from must be rejected"
    );

    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn adding_an_override_for_auth_class_is_rejected() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_pq_override_auth");
    let db_name = unique_name("test_db_pq_override_auth");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    let producer_name = unique_name("producer");
    register_test_producer(&control_pool, &control_url, &slug, &producer_name).await;

    let now = Utc::now();
    let result = add_producer_quota_override(
        &control_pool,
        &control_url,
        &slug,
        &producer_name,
        "sms",
        class::AUTH,
        500,
        now,
        now + chrono::Duration::days(1),
        "ops-lead",
        "campaign day uplift",
        "test-actor",
    )
    .await;
    assert!(
        result.is_err(),
        "an override for class \"auth\" must be rejected"
    );

    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn listing_overrides_returns_every_row_for_the_tenant() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_pq_override_list");
    let db_name = unique_name("test_db_pq_override_list");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    let producer_name = unique_name("producer");
    register_test_producer(&control_pool, &control_url, &slug, &producer_name).await;

    let now = Utc::now();
    add_producer_quota_override(
        &control_pool,
        &control_url,
        &slug,
        &producer_name,
        "sms",
        class::MARKETING,
        500,
        now,
        now + chrono::Duration::days(1),
        "ops-lead",
        "campaign day one",
        "test-actor",
    )
    .await
    .expect("first override failed");
    add_producer_quota_override(
        &control_pool,
        &control_url,
        &slug,
        &producer_name,
        "sms",
        class::MARKETING,
        800,
        now + chrono::Duration::days(2),
        now + chrono::Duration::days(3),
        "ops-lead",
        "campaign day two",
        "test-actor",
    )
    .await
    .expect("second override failed");

    let rows = list_producer_quota_overrides(&control_pool, &control_url, &slug)
        .await
        .expect("listing overrides failed");
    assert_eq!(rows.len(), 2);

    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn flushing_and_rebuilding_preserves_in_progress_window_counts() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_pq_flush_rebuild");
    let db_name = unique_name("test_db_pq_flush_rebuild");
    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name)
            .await;

    let tenant_pool =
        connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 2)
            .await
            .expect("connecting tenant pool failed")
            .pool;

    let producer_name = unique_name("producer");
    let producer_id =
        register_test_producer(&control_pool, &control_url, &slug, &producer_name)
            .await;

    // A hard per_minute limit of 4 turns "did the rebuilt tracker really
    // resume from 3, not 0" into an observable defer/admit boundary, not
    // just an equal-decision coincidence (an unlimited producer would admit
    // either way regardless of whether the count carried over).
    set_producer_quota(
        &control_pool,
        &control_url,
        &slug,
        &producer_name,
        "sms",
        class::MARKETING,
        Some(4),
        None,
        enforcement::HARD,
        "test-actor",
    )
    .await
    .expect("setting producer quota failed");

    let now = Utc::now();
    let original = QuotaTracker::new("UTC");
    original
        .refresh_config(&tenant_pool, now)
        .await
        .expect("refreshing quota config failed");
    for _ in 0..3 {
        let decision = original.check_and_record(producer_id, "sms", "marketing", now);
        assert_eq!(decision, QuotaDecision::Admit);
    }

    original
        .flush(&tenant_pool)
        .await
        .expect("flushing usage failed");

    let usage: Vec<UsageRow> = sqlx::query_as(
        "SELECT producer_id, channel, class, granularity, window_start, sent, blocked \
         FROM producer_usage WHERE producer_id = $1",
    )
    .bind(producer_id)
    .fetch_all(&tenant_pool)
    .await
    .expect("querying producer_usage failed");
    assert_eq!(usage.len(), 2, "one minute row and one day row");
    for row in &usage {
        assert_eq!(row.sent, 3);
        assert_eq!(row.blocked, 0);
        assert!(
            row.granularity == granularity::MINUTE
                || row.granularity == granularity::DAY
        );
    }

    // A fresh tracker, rebuilt from producer_usage rather than from the
    // in-process `original` -- the direct proof of decision 12's
    // restart-safety claim.
    let rebuilt = QuotaTracker::new("UTC");
    rebuilt
        .refresh_config(&tenant_pool, now)
        .await
        .expect("refreshing quota config failed");
    rebuilt
        .rebuild_from_db(&tenant_pool, now)
        .await
        .expect("rebuilding from db failed");

    // The 4th call admits (3 -> 4, right at the limit); the 5th defers. If
    // `rebuild_from_db` had reset the count to zero instead of restoring 3,
    // both calls below would incorrectly admit.
    let fourth = rebuilt.check_and_record(producer_id, "sms", "marketing", now);
    assert_eq!(fourth, QuotaDecision::Admit);
    let fifth = rebuilt.check_and_record(producer_id, "sms", "marketing", now);
    assert!(
        matches!(fifth, QuotaDecision::Defer(_)),
        "a tracker rebuilt from producer_usage must resume counting from the \
         persisted total, not reset to zero (decision 12)"
    );

    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

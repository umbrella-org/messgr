//! `partition_lifecycle` integration suite (DESIGN.md §4.1, §7.2, §7.5,
//! T-014), following `tests/tenant_config.rs`'s conventions: real
//! provisioning against the local stack, no mocks. Requires `just
//! tablespace-init` to have been run once against the local Postgres
//! instance beforehand — these tests assert against the real `messgr_cold`
//! tablespace it creates.

use chrono::{DateTime, Datelike, Months, NaiveDate, Utc};
use sqlx::PgPool;
use sqlx::postgres::types::PgInterval;
use uuid::Uuid;

use messgr::db;
use messgr::keystore::VaultKeyStore;
use messgr::partition_lifecycle::{lifecycle, repo};
use messgr::profile::Profile;
use messgr::tenant::pool::connect_tenant_pool;
use messgr::tenant::provision::provision_tenant;
use messgr::tenant_config::configure::set_tenant_config;
use messgr::tenant_config::model::{TenantConfigInput, verification_mode};
use messgr::tenant_config::repo as tenant_config_repo;

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
        staleness_max_age: PgInterval {
            months: 0,
            days: 0,
            microseconds: 7_200 * 1_000_000,
        },
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
        Profile::Dev,
        "test-actor",
        vault.client(),
    )
    .await
    .expect("provisioning test tenant failed")
    .tenant_id
}

fn month_start(dt: DateTime<Utc>) -> NaiveDate {
    let date = dt.date_naive();
    NaiveDate::from_ymd_opt(date.year(), date.month(), 1)
        .expect("a valid date's own year/month always produces a valid day-1 date")
}

fn months_ago(dt: DateTime<Utc>, months: u32) -> NaiveDate {
    month_start(dt)
        .checked_sub_months(Months::new(months))
        .expect("subtracting a bounded number of months must not underflow")
}

async fn insert_comms_request(pool: &PgPool, created_at: DateTime<Utc>) {
    sqlx::query(
        "INSERT INTO comms_request \
         (tenant_id, id, created_at, customer_id, channel, class, template_id, \
          template_version, destination_hmac, destination_ciphertext, producer_id) \
         VALUES ($1, $2, $3, $4, 'sms', 'transactional', 'balance-alert', 1, $5, $6, $7)",
    )
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .bind(created_at)
    .bind(Uuid::new_v4())
    .bind(vec![0u8; 4])
    .bind(vec![0u8; 4])
    .bind(Uuid::new_v4())
    .execute(pool)
    .await
    .expect("inserting synthetic comms_request row failed");
}

async fn insert_comms_event(pool: &PgPool, occurred_at: DateTime<Utc>) {
    sqlx::query(
        "INSERT INTO comms_event (comms_request_id, customer_id, occurred_at, event_type) \
         VALUES ($1, $2, $3, 'queued')",
    )
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .bind(occurred_at)
    .execute(pool)
    .await
    .expect("inserting synthetic comms_event row failed");
}

async fn partition_tablespace(pool: &PgPool, name: &str) -> Option<String> {
    sqlx::query_scalar(
        "SELECT ts.spcname FROM pg_class c \
         LEFT JOIN pg_tablespace ts ON ts.oid = c.reltablespace \
         WHERE c.relname = $1",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("querying partition tablespace failed")
}

async fn all_indexes_on_tablespace(
    pool: &PgPool,
    table_name: &str,
    tablespace: &str,
) -> bool {
    let (total, on_tablespace): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE ts.spcname = $2) \
         FROM pg_indexes i \
         JOIN pg_class c ON c.relname = i.indexname \
         LEFT JOIN pg_tablespace ts ON ts.oid = c.reltablespace \
         WHERE i.tablename = $1",
    )
    .bind(table_name)
    .bind(tablespace)
    .fetch_one(pool)
    .await
    .expect("querying index tablespaces failed");
    total > 0 && total == on_tablespace
}

async fn is_attached(pool: &PgPool, parent: &str, child: &str) -> bool {
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_inherits i \
         JOIN pg_class p ON p.oid = i.inhparent \
         JOIN pg_class c ON c.oid = i.inhrelid \
         WHERE p.relname = $1 AND c.relname = $2",
    )
    .bind(parent)
    .bind(child)
    .fetch_one(pool)
    .await
    .expect("querying pg_inherits failed");
    count == 1
}

#[tokio::test]
async fn create_ahead_is_idempotent_after_provisioning() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_plc_idempotent");
    let db_name = unique_name("test_plc_idempotent_db");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    let tenant_pool = connect_tenant_pool(&control_url, &db_name, 5, Profile::Dev)
        .await
        .expect("connecting tenant pool failed");

    let as_of = Utc::now();
    let first = lifecycle::run(&tenant_pool, as_of, None)
        .await
        .expect("first run failed");
    let second = lifecycle::run(&tenant_pool, as_of, None)
        .await
        .expect("second run failed");

    assert!(
        first.created.is_empty(),
        "T-009 already bootstraps current+next month at provisioning time: {:?}",
        first.created
    );
    assert!(second.created.is_empty());
    assert!(first.retention_skipped);
    assert!(second.retention_skipped);

    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn the_next_month_bootstrap_partition_accepts_a_real_insert() {
    // T-009/F1 (review finding) noted that its own bootstrap only ever
    // verified the next-month partition's *existence* via `pg_inherits`,
    // never inserted a row into it, and deferred that data-level coverage
    // to T-014's own test suite -- this closes that gap for the create-ahead
    // logic this ticket ships.
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_plc_next_month");
    let db_name = unique_name("test_plc_next_month_db");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    let tenant_pool = connect_tenant_pool(&control_url, &db_name, 5, Profile::Dev)
        .await
        .expect("connecting tenant pool failed");

    let as_of = Utc::now();
    lifecycle::run(&tenant_pool, as_of, None)
        .await
        .expect("run failed");

    let next_month = month_start(as_of)
        .checked_add_months(Months::new(1))
        .expect("valid date");
    let next_month_timestamp = next_month
        .and_hms_opt(12, 0, 0)
        .expect("valid time")
        .and_utc();

    insert_comms_request(&tenant_pool, next_month_timestamp).await;
    insert_comms_event(&tenant_pool, next_month_timestamp).await;

    let request_name = repo::partition_name("comms_request", next_month);
    let event_name = repo::partition_name("comms_event", next_month);

    let request_count: i64 =
        sqlx::query_scalar(&format!("SELECT count(*) FROM {request_name}"))
            .fetch_one(&tenant_pool)
            .await
            .expect("querying the next-month comms_request partition failed");
    assert_eq!(request_count, 1);

    let event_count: i64 =
        sqlx::query_scalar(&format!("SELECT count(*) FROM {event_name}"))
            .fetch_one(&tenant_pool)
            .await
            .expect("querying the next-month comms_event partition failed");
    assert_eq!(event_count, 1);

    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn a_partition_inside_the_retention_window_moves_but_is_not_dropped() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_plc_move");
    let db_name = unique_name("test_plc_move_db");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    let tenant_pool = connect_tenant_pool(&control_url, &db_name, 5, Profile::Dev)
        .await
        .expect("connecting tenant pool failed");

    let as_of = Utc::now();
    let old_month = months_ago(as_of, 20); // inside a 7-year (84-month) retention window
    let request_name = repo::partition_name("comms_request", old_month);
    let event_name = repo::partition_name("comms_event", old_month);
    repo::create_partition(&tenant_pool, "comms_request", old_month)
        .await
        .expect("creating synthetic comms_request partition failed");
    repo::create_partition(&tenant_pool, "comms_event", old_month)
        .await
        .expect("creating synthetic comms_event partition failed");

    let old_timestamp = old_month
        .and_hms_opt(12, 0, 0)
        .expect("valid time")
        .and_utc();
    insert_comms_request(&tenant_pool, old_timestamp).await;
    insert_comms_event(&tenant_pool, old_timestamp).await;

    set_tenant_config(
        &control_pool,
        &control_url,
        &slug,
        sample_input(7),
        Profile::Dev,
        "test-actor",
    )
    .await
    .expect("setting tenant_config failed");
    let config = tenant_config_repo::load(&tenant_pool)
        .await
        .expect("loading tenant_config failed")
        .expect("tenant_config row must exist");

    let report = lifecycle::run(&tenant_pool, as_of, Some(&config))
        .await
        .expect("run failed");

    assert!(report.moved.contains(&request_name), "{:?}", report.moved);
    assert!(report.moved.contains(&event_name), "{:?}", report.moved);
    assert!(report.dropped.is_empty(), "{:?}", report.dropped);
    assert!(!report.retention_skipped);

    assert_eq!(
        partition_tablespace(&tenant_pool, &request_name).await,
        Some(lifecycle::COLD_TABLESPACE.to_string())
    );
    assert_eq!(
        partition_tablespace(&tenant_pool, &event_name).await,
        Some(lifecycle::COLD_TABLESPACE.to_string())
    );
    assert!(
        all_indexes_on_tablespace(
            &tenant_pool,
            &request_name,
            lifecycle::COLD_TABLESPACE
        )
        .await
    );
    assert!(
        all_indexes_on_tablespace(
            &tenant_pool,
            &event_name,
            lifecycle::COLD_TABLESPACE
        )
        .await
    );

    assert!(is_attached(&tenant_pool, "comms_request", &request_name).await);
    assert!(is_attached(&tenant_pool, "comms_event", &event_name).await);

    let request_count: i64 =
        sqlx::query_scalar(&format!("SELECT count(*) FROM {request_name}"))
            .fetch_one(&tenant_pool)
            .await
            .expect("querying moved comms_request partition failed");
    assert_eq!(request_count, 1, "the row must survive the tablespace move");

    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn a_partition_past_the_retention_boundary_is_detached_and_dropped() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_plc_drop");
    let db_name = unique_name("test_plc_drop_db");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    let tenant_pool = connect_tenant_pool(&control_url, &db_name, 5, Profile::Dev)
        .await
        .expect("connecting tenant pool failed");

    let as_of = Utc::now();
    let ancient_month = months_ago(as_of, 96); // 8 years -- past a 7-year (84-month) retention
    let request_name = repo::partition_name("comms_request", ancient_month);
    let event_name = repo::partition_name("comms_event", ancient_month);
    repo::create_partition(&tenant_pool, "comms_request", ancient_month)
        .await
        .expect("creating synthetic comms_request partition failed");
    repo::create_partition(&tenant_pool, "comms_event", ancient_month)
        .await
        .expect("creating synthetic comms_event partition failed");

    set_tenant_config(
        &control_pool,
        &control_url,
        &slug,
        sample_input(7),
        Profile::Dev,
        "test-actor",
    )
    .await
    .expect("setting tenant_config failed");
    let config = tenant_config_repo::load(&tenant_pool)
        .await
        .expect("loading tenant_config failed")
        .expect("tenant_config row must exist");

    let report = lifecycle::run(&tenant_pool, as_of, Some(&config))
        .await
        .expect("run failed");

    assert!(
        report.dropped.contains(&request_name),
        "{:?}",
        report.dropped
    );
    assert!(report.dropped.contains(&event_name), "{:?}", report.dropped);
    assert!(!report.retention_skipped);

    assert!(
        !repo::partition_exists(&tenant_pool, &request_name)
            .await
            .unwrap()
    );
    assert!(
        !repo::partition_exists(&tenant_pool, &event_name)
            .await
            .unwrap()
    );
    assert!(!is_attached(&tenant_pool, "comms_request", &request_name).await);
    assert!(!is_attached(&tenant_pool, "comms_event", &event_name).await);

    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn no_tenant_config_means_no_drop_ever() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_plc_unconfigured");
    let db_name = unique_name("test_plc_unconfigured_db");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    let tenant_pool = connect_tenant_pool(&control_url, &db_name, 5, Profile::Dev)
        .await
        .expect("connecting tenant pool failed");

    let as_of = Utc::now();
    let ancient_month = months_ago(as_of, 96);
    let request_name = repo::partition_name("comms_request", ancient_month);
    repo::create_partition(&tenant_pool, "comms_request", ancient_month)
        .await
        .expect("creating synthetic comms_request partition failed");

    // No tenant_config row is ever set for this tenant.
    let report = lifecycle::run(&tenant_pool, as_of, None)
        .await
        .expect("run failed");

    assert!(report.dropped.is_empty(), "{:?}", report.dropped);
    assert!(report.retention_skipped);
    assert!(
        repo::partition_exists(&tenant_pool, &request_name)
            .await
            .unwrap()
    );
    assert!(is_attached(&tenant_pool, "comms_request", &request_name).await);

    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn a_non_conforming_partition_name_is_never_touched() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_plc_nonconforming");
    let db_name = unique_name("test_plc_nonconforming_db");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    let tenant_pool = connect_tenant_pool(&control_url, &db_name, 5, Profile::Dev)
        .await
        .expect("connecting tenant pool failed");

    let as_of = Utc::now();
    let ancient_month = months_ago(as_of, 96);
    let month_end = ancient_month
        .checked_add_months(Months::new(1))
        .expect("valid date");
    sqlx::query(&format!(
        "CREATE TABLE comms_request_legacy PARTITION OF comms_request \
         FOR VALUES FROM ('{ancient_month}') TO ('{month_end}')"
    ))
    .execute(&tenant_pool)
    .await
    .expect("creating a non-conforming partition failed");

    set_tenant_config(
        &control_pool,
        &control_url,
        &slug,
        sample_input(1), // 12-month retention -- well below this partition's age
        Profile::Dev,
        "test-actor",
    )
    .await
    .expect("setting tenant_config failed");
    let config = tenant_config_repo::load(&tenant_pool)
        .await
        .expect("loading tenant_config failed")
        .expect("tenant_config row must exist");

    let report = lifecycle::run(&tenant_pool, as_of, Some(&config))
        .await
        .expect("run failed");

    assert!(!report.moved.contains(&"comms_request_legacy".to_string()));
    assert!(!report.dropped.contains(&"comms_request_legacy".to_string()));
    assert!(is_attached(&tenant_pool, "comms_request", "comms_request_legacy").await);
    assert_eq!(
        partition_tablespace(&tenant_pool, "comms_request_legacy").await,
        None,
        "a non-conforming partition must be left on the default tablespace"
    );

    tenant_pool.close().await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

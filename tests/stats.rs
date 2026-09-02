//! `messgr-control stats` integration suite (DESIGN.md §11.4, T-028),
//! following `tests/ledger_outbox_schema.rs`'s conventions: real
//! provisioning against the local stack, no mocks, its own copies of the
//! shared test helpers (this project does not share test helpers across
//! files). Only `sent`/`failed`/`expired`/`pending` are used as
//! `final_status` values -- the vocabulary the dispatcher actually writes
//! today (`src/dispatcher/worker.rs`, `src/dispatcher/drain.rs`), not the
//! full aspirational list in DESIGN.md §4.4.

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use messgr::db;
use messgr::keystore::VaultKeyStore;
use messgr::profile::Profile;
use messgr::stats::tenant_message_stats;
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

struct TestTenant {
    control_pool: PgPool,
    tenant_pool: PgPool,
    control_url: String,
    slug: String,
    db_name: String,
}

impl TestTenant {
    async fn provision(prefix: &str) -> Self {
        let control_url = control_database_url();
        let control_pool = db::connect(&control_url, 5)
            .await
            .expect("failed to connect to control database");
        let vault = vault_keystore();

        let slug = unique_name(&format!("test_st_{prefix}"));
        let db_name = unique_name(&format!("test_db_st_{prefix}"));
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name)
            .await;

        let tenant_pool = connect_tenant_pool(&control_url, &db_name, 5, Profile::Dev)
            .await
            .expect("connecting tenant pool failed");

        TestTenant {
            control_pool,
            tenant_pool,
            control_url,
            slug,
            db_name,
        }
    }

    async fn teardown(self) {
        self.tenant_pool.close().await;
        drop_test_tenant(&self.control_pool, &self.db_name, &self.slug).await;
    }
}

async fn insert_comms_request(
    pool: &PgPool,
    id: Uuid,
    created_at: DateTime<Utc>,
    channel: &str,
    final_status: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO comms_request (
            tenant_id, id, created_at, customer_id, channel, class, template_id,
            template_version, destination_hmac, destination_ciphertext, producer_id,
            final_status
        ) VALUES (
            $1, $2, $3, $4, $5, 'transactional', 'welcome', 1, $6, $7, $8, $9
        )
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(id)
    .bind(created_at)
    .bind(Uuid::new_v4())
    .bind(channel)
    .bind(b"hmac".to_vec())
    .bind(b"ciphertext".to_vec())
    .bind(Uuid::new_v4())
    .bind(final_status)
    .execute(pool)
    .await
    .map(|_| ())
}

fn count_of(
    rows: &[messgr::stats::ChannelStatusCount],
    channel: &str,
    status: &str,
) -> Option<i64> {
    rows.iter()
        .find(|r| r.channel == channel && r.status == status)
        .map(|r| r.count)
}

#[tokio::test]
async fn stats_reports_channel_and_status_counts_excluding_whatsapp() {
    let tenant = TestTenant::provision("counts").await;
    let now = Utc::now();

    insert_comms_request(
        &tenant.tenant_pool,
        Uuid::new_v4(),
        now,
        "sms",
        Some("sent"),
    )
    .await
    .expect("insert sms/sent #1 failed");
    insert_comms_request(
        &tenant.tenant_pool,
        Uuid::new_v4(),
        now,
        "sms",
        Some("sent"),
    )
    .await
    .expect("insert sms/sent #2 failed");
    insert_comms_request(&tenant.tenant_pool, Uuid::new_v4(), now, "sms", None)
        .await
        .expect("insert sms/pending failed");
    insert_comms_request(
        &tenant.tenant_pool,
        Uuid::new_v4(),
        now,
        "email",
        Some("failed"),
    )
    .await
    .expect("insert email/failed failed");
    insert_comms_request(
        &tenant.tenant_pool,
        Uuid::new_v4(),
        now,
        "whatsapp",
        Some("sent"),
    )
    .await
    .expect("insert whatsapp/sent failed");

    let rows = tenant_message_stats(
        &tenant.control_pool,
        &tenant.control_url,
        &tenant.slug,
        None,
        Profile::Dev,
    )
    .await
    .expect("tenant_message_stats failed");

    assert_eq!(
        count_of(&rows, "sms", "sent"),
        Some(2),
        "expected 2 sms/sent rows"
    );
    assert_eq!(
        count_of(&rows, "sms", "pending"),
        Some(1),
        "a NULL final_status must be reported as status=pending"
    );
    assert_eq!(
        count_of(&rows, "email", "failed"),
        Some(1),
        "expected 1 email/failed row"
    );
    assert!(
        rows.iter().all(|r| r.channel != "whatsapp"),
        "whatsapp rows must never appear in the result: {rows:?}"
    );

    tenant.teardown().await;
}

#[tokio::test]
async fn stats_since_filter_is_exact_at_the_day_boundary() {
    let tenant = TestTenant::provision("since").await;
    let today = Utc::now();
    let tomorrow = today + Duration::days(1);

    insert_comms_request(
        &tenant.tenant_pool,
        Uuid::new_v4(),
        today,
        "sms",
        Some("sent"),
    )
    .await
    .expect("insert today's row failed");
    insert_comms_request(
        &tenant.tenant_pool,
        Uuid::new_v4(),
        tomorrow,
        "sms",
        Some("sent"),
    )
    .await
    .expect("insert tomorrow's row failed");

    let all_time = tenant_message_stats(
        &tenant.control_pool,
        &tenant.control_url,
        &tenant.slug,
        None,
        Profile::Dev,
    )
    .await
    .expect("tenant_message_stats (all-time) failed");
    assert_eq!(
        count_of(&all_time, "sms", "sent"),
        Some(2),
        "with no --since both rows must be counted"
    );

    let from_tomorrow = tenant_message_stats(
        &tenant.control_pool,
        &tenant.control_url,
        &tenant.slug,
        Some(tomorrow.date_naive()),
        Profile::Dev,
    )
    .await
    .expect("tenant_message_stats (since tomorrow) failed");
    assert_eq!(
        count_of(&from_tomorrow, "sms", "sent"),
        Some(1),
        "--since tomorrow's date must exclude today's row and include tomorrow's"
    );

    let from_day_after = tenant_message_stats(
        &tenant.control_pool,
        &tenant.control_url,
        &tenant.slug,
        Some((tomorrow + Duration::days(1)).date_naive()),
        Profile::Dev,
    )
    .await
    .expect("tenant_message_stats (since day after tomorrow) failed");
    assert_eq!(
        count_of(&from_day_after, "sms", "sent"),
        None,
        "--since a date after both rows must exclude everything"
    );

    tenant.teardown().await;
}

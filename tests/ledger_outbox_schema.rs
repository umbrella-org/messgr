//! `comms_request`/`outbox`/`comms_event`/`idempotency` schema integration
//! suite (DESIGN.md §4.1-4.4, T-009), following `tests/customer_dek.rs`'s
//! conventions: real provisioning against the local stack, no mocks. No
//! model/repo layer (T-009 decision 5) — every query here is raw SQL.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use messgr::db;
use messgr::keystore::VaultKeyStore;
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
    let outcome = provision_tenant(
        control_pool,
        control_url,
        slug,
        "eu",
        database_name,
        "test-actor",
        vault.client(),
    )
    .await
    .expect("provisioning test tenant failed");
    outcome.tenant_id
}

struct TestTenant {
    control_pool: PgPool,
    tenant_pool: PgPool,
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

        // Postgres identifiers cap at 63 bytes (NAMEDATALEN); db_name is one
        // (it becomes the literal database name), so keep the base short —
        // "test_db_ledger_outbox_" plus a longer prefix plus the 32-hex
        // suffix overflowed that limit and got silently truncated by
        // Postgres, which then failed connect_tenant_pool's
        // current_database() tripwire (db.rs) rather than the schema itself.
        let slug = unique_name(&format!("test_lo_{prefix}"));
        let db_name = unique_name(&format!("test_db_lo_{prefix}"));
        let tenant_id = provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name)
            .await;

        let tenant_pool = connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 5)
            .await
            .expect("connecting tenant pool failed")
            .pool;

        TestTenant {
            control_pool,
            tenant_pool,
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
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO comms_request (
            tenant_id, id, created_at, customer_id, channel, class, template_id,
            template_version, destination_hmac, destination_ciphertext, producer_id
        ) VALUES (
            $1, $2, $3, $4, 'sms', 'transactional', 'welcome', 1, $5, $6, $7
        )
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(id)
    .bind(created_at)
    .bind(Uuid::new_v4())
    .bind(b"hmac".to_vec())
    .bind(b"ciphertext".to_vec())
    .bind(Uuid::new_v4())
    .execute(pool)
    .await
    .map(|_| ())
}

#[tokio::test]
async fn bootstrap_partitions_exist_for_current_and_next_month() {
    let tenant = TestTenant::provision("bootstrap").await;

    for relation in ["comms_request", "comms_event"] {
        let query = format!(
            "SELECT count(*) FROM pg_inherits WHERE inhparent = '{relation}'::regclass"
        );
        let (count,): (i64,) = sqlx::query_as(&query)
            .fetch_one(&tenant.tenant_pool)
            .await
            .unwrap_or_else(|_| panic!("counting partitions of {relation} failed"));
        assert_eq!(
            count, 2,
            "{relation} must have exactly 2 bootstrap partitions"
        );
    }

    tenant.teardown().await;
}

#[tokio::test]
async fn insert_into_a_bootstrapped_partition_round_trips() {
    let tenant = TestTenant::provision("insert_ok").await;

    let id = Uuid::new_v4();
    let created_at = Utc::now();
    insert_comms_request(&tenant.tenant_pool, id, created_at)
        .await
        .expect("insert into the current-month partition must succeed");

    let row: (Uuid,) = sqlx::query_as(
        "SELECT id FROM comms_request WHERE created_at = $1 AND id = $2",
    )
    .bind(created_at)
    .bind(id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("round-trip select failed");
    assert_eq!(row.0, id);

    tenant.teardown().await;
}

#[tokio::test]
async fn insert_outside_any_bootstrapped_partition_is_rejected() {
    let tenant = TestTenant::provision("no_partition").await;

    let id = Uuid::new_v4();
    let created_at = Utc::now() + chrono::Duration::days(90);
    let result = insert_comms_request(&tenant.tenant_pool, id, created_at).await;
    assert!(
        result.is_err(),
        "an INSERT with created_at outside the bootstrapped partitions must fail, not land silently"
    );

    tenant.teardown().await;
}

#[tokio::test]
async fn idempotency_key_is_globally_unique() {
    let tenant = TestTenant::provision("idempotency").await;

    let key = unique_name("idem-key");
    sqlx::query(
        "INSERT INTO idempotency (key, comms_request_id, expires_at) VALUES ($1, $2, now() + interval '30 days')",
    )
    .bind(&key)
    .bind(Uuid::new_v4())
    .execute(&tenant.tenant_pool)
    .await
    .expect("first idempotency insert must succeed");

    let result = sqlx::query(
        "INSERT INTO idempotency (key, comms_request_id, expires_at) VALUES ($1, $2, now() + interval '30 days')",
    )
    .bind(&key)
    .bind(Uuid::new_v4())
    .execute(&tenant.tenant_pool)
    .await;
    assert!(
        result.is_err(),
        "a second insert with the same idempotency key must violate the primary key"
    );

    tenant.teardown().await;
}

async fn insert_outbox_row(
    pool: &PgPool,
    comms_request_id: Uuid,
    channel: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO outbox (
            comms_request_id, created_at, channel, class, priority,
            customer_id, address_id, producer_id, next_attempt_at
        ) VALUES (
            $1, now(), $2, 'transactional', 1, $3, $4, $5, now()
        )
        "#,
    )
    .bind(comms_request_id)
    .bind(channel)
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .execute(pool)
    .await
    .map(|_| ())
}

#[tokio::test]
async fn outbox_claim_query_skips_a_row_locked_by_another_connection() {
    let tenant = TestTenant::provision("claim_skip").await;
    let channel = "sms";

    let locked_id = Uuid::new_v4();
    let claimable_id = Uuid::new_v4();
    insert_outbox_row(&tenant.tenant_pool, locked_id, channel)
        .await
        .expect("inserting the row to be locked failed");
    insert_outbox_row(&tenant.tenant_pool, claimable_id, channel)
        .await
        .expect("inserting the claimable row failed");

    // Hold locked_id under FOR UPDATE in an open, uncommitted transaction on
    // its own connection — simulating another dispatcher already claiming it.
    let mut locker = tenant
        .tenant_pool
        .begin()
        .await
        .expect("beginning the locking transaction failed");
    sqlx::query(
        "SELECT comms_request_id FROM outbox WHERE comms_request_id = $1 FOR UPDATE",
    )
    .bind(locked_id)
    .fetch_one(&mut *locker)
    .await
    .expect("locking the row failed");

    // §4.2's claim query, run from a different connection via the pool.
    let claimed: (Uuid,) = sqlx::query_as(
        r#"
        UPDATE outbox SET leased_until = now() + interval '2 minutes'
        WHERE comms_request_id IN (
            SELECT comms_request_id FROM outbox
            WHERE channel = $1 AND next_attempt_at <= now() AND leased_until IS NULL
            ORDER BY priority, next_attempt_at
            LIMIT 1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING comms_request_id
        "#,
    )
    .bind(channel)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("claim query must skip the locked row and claim the other one");

    assert_eq!(
        claimed.0, claimable_id,
        "the claim query must skip the row locked by the other connection"
    );

    locker
        .rollback()
        .await
        .expect("rolling back the locker failed");
    tenant.teardown().await;
}

#[tokio::test]
async fn comms_event_uniqueness_is_enforced_per_provider_ref() {
    let tenant = TestTenant::provision("event_unique").await;

    let comms_request_id = Uuid::new_v4();
    let customer_id = Uuid::new_v4();
    let occurred_at = Utc::now();

    let insert = |provider_ref: &'static str| {
        sqlx::query(
            r#"
            INSERT INTO comms_event (comms_request_id, customer_id, occurred_at, event_type, provider_ref)
            VALUES ($1, $2, $3, 'sent', $4)
            "#,
        )
        .bind(comms_request_id)
        .bind(customer_id)
        .bind(occurred_at)
        .bind(provider_ref)
    };

    insert("ref-a")
        .execute(&tenant.tenant_pool)
        .await
        .expect("first event insert must succeed");

    let duplicate = insert("ref-a").execute(&tenant.tenant_pool).await;
    assert!(
        duplicate.is_err(),
        "an identical (occurred_at, comms_request_id, event_type, provider_ref) tuple must violate the UNIQUE constraint"
    );

    insert("ref-b")
        .execute(&tenant.tenant_pool)
        .await
        .expect("a different provider_ref must be allowed to coexist");

    tenant.teardown().await;
}

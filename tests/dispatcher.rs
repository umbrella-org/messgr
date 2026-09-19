//! Dispatcher integration suite (DESIGN.md §4.1, §4.2, §4.4, §9, T-013),
//! following `tests/ledger_outbox_schema.rs`/`tests/provider_config.rs`'s
//! conventions: real provisioning against the local stack, no mocks except
//! the provider itself (`wiremock`, the same tool T-012's `HttpSender`
//! tests use).

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use messgr::customer_dek::lifecycle::get_or_create_dek;
use messgr::db;
use messgr::dispatcher::model::ClaimedOutbox;
use messgr::dispatcher::repo;
use messgr::dispatcher::worker::{DispatcherContext, try_process};
use messgr::encryption;
use messgr::ingest::repo::insert_transactional;
use messgr::key_cache::KeyCache;
use messgr::keystore::VaultKeyStore;
use messgr::kill_switch::cache::{ChannelExclusion, KillSwitchCache};
use messgr::producer::register::register_producer;
use messgr::producer_quota::configure::set_producer_quota;
use messgr::producer_quota::model::enforcement;
use messgr::producer_quota::tracker::QuotaTracker;
use messgr::profile::Profile;
use messgr::quiet_hours::model::QuietHoursPolicy;
use messgr::sender::Sender;
use messgr::sender::http::HttpSender;
use messgr::tenant::pool::connect_tenant_pool;
use messgr::tenant::provision::provision_tenant;

/// No switches engaged — every pre-T-016 test in this file claims against an
/// empty kill-switch world.
fn no_exclusion() -> ChannelExclusion {
    ChannelExclusion::default()
}

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

struct TestTenant {
    control_pool: PgPool,
    tenant_pool: PgPool,
    slug: String,
    database_name: String,
    mount: String,
}

async fn provision_test_tenant(vault: &VaultKeyStore) -> TestTenant {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");

    let slug = unique_name("test_dispatcher");
    let database_name = unique_name("test_db_dispatcher");

    let provision_outcome = provision_tenant(
        &control_pool,
        &control_url,
        &slug,
        "eu",
        &database_name,
        "test-actor",
        vault.client(),
    )
    .await
    .expect("provisioning test tenant failed");

    let tenant_pool = connect_tenant_pool(
        &control_pool,
        &control_url,
        provision_outcome.tenant_id,
        &database_name,
        5,
    )
    .await
    .expect("connecting tenant pool failed")
    .pool;

    TestTenant {
        control_pool,
        tenant_pool,
        slug: slug.clone(),
        database_name,
        mount: format!("transit/{slug}"),
    }
}

impl TestTenant {
    async fn cleanup(self) {
        self.tenant_pool.close().await;
        drop_test_tenant(&self.control_pool, &self.database_name, &self.slug).await;
    }
}

/// Inserts a `customer` row and a `customer_address` row under it directly
/// (T-036) — raw SQL, not `customer::repo::insert_address` (which always
/// writes `verified_at = NULL` and needs a transaction the dispatcher-suite
/// callers don't have open), since these tests need to control
/// `verified_at` explicitly. `customer_address.customer_id` has a real FK
/// to `customer(id)` (migration 0008), so the parent row is mandatory —
/// this suite otherwise never creates one, unlike `tests/customer.rs`.
async fn insert_customer_address(
    tenant: &TestTenant,
    id: Uuid,
    customer_id: Uuid,
    value_ciphertext: &[u8],
    verified_at: Option<DateTime<Utc>>,
) {
    let now = Utc::now();
    sqlx::query(
        "INSERT INTO customer (id, locale, timezone, provisional, created_at) \
         VALUES ($1, 'en-US', 'UTC', false, $2)",
    )
    .bind(customer_id)
    .bind(now)
    .execute(&tenant.tenant_pool)
    .await
    .expect("inserting customer failed");

    sqlx::query(
        r#"
        INSERT INTO customer_address (
            id, customer_id, kind, value_ciphertext, value_hmac, rank, label,
            verified_at, active_from, active_to, source_updated_at
        ) VALUES ($1, $2, 'msisdn', $3, $4, 1, NULL, $5, $6, NULL, $6)
        "#,
    )
    .bind(id)
    .bind(customer_id)
    .bind(value_ciphertext)
    .bind(unique_name("hmac").into_bytes())
    .bind(verified_at)
    .bind(now)
    .execute(&tenant.tenant_pool)
    .await
    .expect("inserting customer_address failed");
}

/// Writes a real, ready-to-claim `outbox` row the same way `messgr-ingest`
/// would: a real DEK, real AES-256-GCM ciphertexts, a matching
/// `customer_address` row (T-036 — the verification gate needs one to
/// check), and a single-transaction `comms_request` + `outbox` insert
/// (`ingest::repo::insert_transactional`) — rather than standing up
/// mTLS/axum, since this suite is about the dispatcher's read/decrypt/
/// send/write path, not ingest's.
#[allow(clippy::too_many_arguments)]
async fn write_outbox_row_with_verification(
    tenant: &TestTenant,
    vault: &VaultKeyStore,
    cache: &KeyCache,
    destination: &str,
    body: &str,
    class: &str,
    verified_at: Option<DateTime<Utc>>,
    destination_hmac: &[u8],
    expires_at: Option<DateTime<Utc>>,
    producer_id: Uuid,
) -> (Uuid, DateTime<Utc>, Uuid) {
    let tenant_id =
        messgr::tenant::repo::find_by_slug(&tenant.control_pool, &tenant.slug)
            .await
            .expect("tenant lookup failed")
            .expect("tenant must exist")
            .id;

    let customer_id = Uuid::new_v4();
    let dek = get_or_create_dek(
        &tenant.tenant_pool,
        vault,
        cache,
        &tenant.mount,
        customer_id,
    )
    .await
    .expect("get_or_create_dek failed");

    let comms_request_id = Uuid::new_v4();
    let aad = comms_request_id.as_bytes();
    let destination_ciphertext = encryption::encrypt(&dek, aad, destination.as_bytes())
        .expect("encrypting destination failed");
    let payload_ciphertext = encryption::encrypt(&dek, aad, body.as_bytes())
        .expect("encrypting payload failed");

    let address_id = Uuid::new_v4();
    insert_customer_address(
        tenant,
        address_id,
        customer_id,
        &destination_ciphertext,
        verified_at,
    )
    .await;

    let outcome = insert_transactional(
        &tenant.tenant_pool,
        &unique_name("idempotency-key"),
        comms_request_id,
        tenant_id,
        customer_id,
        "sms",
        class,
        1,
        "balance-alert",
        1,
        None,
        destination_hmac,
        &destination_ciphertext,
        &payload_ciphertext,
        producer_id,
        address_id,
        None,
        expires_at,
    )
    .await
    .expect("insert_transactional failed");

    let created_at: DateTime<Utc> = match outcome {
        messgr::ingest::repo::InsertOutcome::Created => {
            sqlx::query_scalar("SELECT created_at FROM comms_request WHERE id = $1")
                .bind(comms_request_id)
                .fetch_one(&tenant.tenant_pool)
                .await
                .expect("fetching created_at failed")
        }
        messgr::ingest::repo::InsertOutcome::Replayed { .. } => {
            panic!("a fresh idempotency key must never replay")
        }
    };

    (comms_request_id, created_at, customer_id)
}

/// Every pre-T-036 test in this file wants an ordinary, already-verified
/// send — the verification gate is not what they're testing — so this
/// keeps their call sites unchanged and defaults to `verified_at =
/// Some(now)`, `class = "transactional"`.
async fn write_ready_outbox_row(
    tenant: &TestTenant,
    vault: &VaultKeyStore,
    cache: &KeyCache,
    destination: &str,
    body: &str,
) -> (Uuid, DateTime<Utc>, Uuid) {
    write_outbox_row_with_verification(
        tenant,
        vault,
        cache,
        destination,
        body,
        "transactional",
        Some(Utc::now()),
        b"unused-hmac",
        None,
        Uuid::new_v4(),
    )
    .await
}

/// Same as `write_ready_outbox_row`, but takes `destination_hmac` as a
/// parameter instead of the hardcoded `b"unused-hmac"` literal, so the
/// suppression-gate tests (T-038) can write a row whose `comms_request`
/// joins a `suppression` row on that exact hash.
async fn write_outbox_row_with_hmac(
    tenant: &TestTenant,
    vault: &VaultKeyStore,
    cache: &KeyCache,
    destination: &str,
    body: &str,
    destination_hmac: &[u8],
) -> (Uuid, DateTime<Utc>, Uuid) {
    write_outbox_row_with_verification(
        tenant,
        vault,
        cache,
        destination,
        body,
        "transactional",
        Some(Utc::now()),
        destination_hmac,
        None,
        Uuid::new_v4(),
    )
    .await
}

fn small_cache() -> KeyCache {
    KeyCache::new(NonZeroUsize::new(8).unwrap(), Duration::from_secs(60))
}

/// Same shape as `write_ready_outbox_row`, but threads `channel` through to
/// `insert_transactional` instead of hardcoding `"sms"` — T-046 needs one
/// outbox row per non-SMS channel, and retrofitting `channel` onto
/// `write_outbox_row_with_verification` would touch its ~15 existing
/// SMS-only call sites for nothing.
#[allow(clippy::too_many_arguments)]
async fn write_ready_outbox_row_for_channel(
    tenant: &TestTenant,
    vault: &VaultKeyStore,
    cache: &KeyCache,
    channel: &str,
    destination: &str,
    body: &str,
) -> (Uuid, DateTime<Utc>, Uuid) {
    let tenant_id =
        messgr::tenant::repo::find_by_slug(&tenant.control_pool, &tenant.slug)
            .await
            .expect("tenant lookup failed")
            .expect("tenant must exist")
            .id;

    let customer_id = Uuid::new_v4();
    let dek = get_or_create_dek(
        &tenant.tenant_pool,
        vault,
        cache,
        &tenant.mount,
        customer_id,
    )
    .await
    .expect("get_or_create_dek failed");

    let comms_request_id = Uuid::new_v4();
    let aad = comms_request_id.as_bytes();
    let destination_ciphertext = encryption::encrypt(&dek, aad, destination.as_bytes())
        .expect("encrypting destination failed");
    let payload_ciphertext = encryption::encrypt(&dek, aad, body.as_bytes())
        .expect("encrypting payload failed");

    let address_id = Uuid::new_v4();
    insert_customer_address(
        tenant,
        address_id,
        customer_id,
        &destination_ciphertext,
        Some(Utc::now()),
    )
    .await;

    let outcome = insert_transactional(
        &tenant.tenant_pool,
        &unique_name("idempotency-key"),
        comms_request_id,
        tenant_id,
        customer_id,
        channel,
        "transactional",
        1,
        "balance-alert",
        1,
        None,
        b"unused-hmac",
        &destination_ciphertext,
        &payload_ciphertext,
        Uuid::new_v4(),
        address_id,
        None,
        None,
    )
    .await
    .expect("insert_transactional failed");

    let created_at: DateTime<Utc> = match outcome {
        messgr::ingest::repo::InsertOutcome::Created => {
            sqlx::query_scalar("SELECT created_at FROM comms_request WHERE id = $1")
                .bind(comms_request_id)
                .fetch_one(&tenant.tenant_pool)
                .await
                .expect("fetching created_at failed")
        }
        messgr::ingest::repo::InsertOutcome::Replayed { .. } => {
            panic!("a fresh idempotency key must never replay")
        }
    };

    (comms_request_id, created_at, customer_id)
}

/// T-046: proves `try_process`'s claim/decrypt/send/write gate chain has no
/// hidden SMS-only assumption — mirrors
/// `successful_send_writes_sent_event_and_final_status_and_deletes_the_outbox_row`
/// for a non-SMS channel.
async fn assert_successful_send_for_channel(channel: &str, destination: &str) {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": format!("msg-sent-{channel}-1"),
            "status": "queued",
        })))
        .mount(&mock_server)
        .await;

    let (comms_request_id, created_at, customer_id) =
        write_ready_outbox_row_for_channel(
            &tenant,
            &vault,
            &cache,
            channel,
            destination,
            "hello there",
        )
        .await;

    let sender: Arc<dyn Sender> =
        Arc::new(HttpSender::new(mock_server.uri(), "test-key".to_string()));
    let ctx = DispatcherContext {
        pool: tenant.tenant_pool.clone(),
        keystore: Arc::new(vault_keystore()),
        cache: Arc::new(cache),
        mount: tenant.mount.clone(),
        sender,
        verification_mode: "observe".to_string(),
        default_timezone: "UTC".to_string(),
        quiet_hours_policy: None,
        kill_switches: Arc::new(KillSwitchCache::new()),
        draining: Arc::new(RwLock::new(HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(QuotaTracker::new("UTC")),
    };

    let claimed = repo::claim(
        &tenant.tenant_pool,
        channel,
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].comms_request_id, comms_request_id);

    try_process(&ctx, &claimed[0])
        .await
        .expect("try_process failed");

    let final_status: Option<String> = sqlx::query_scalar(
        "SELECT final_status FROM comms_request WHERE created_at = $1 AND id = $2",
    )
    .bind(created_at)
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching final_status failed");
    assert_eq!(final_status.as_deref(), Some("sent"));

    let events: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT event_type, provider_ref, provider_status FROM comms_event WHERE comms_request_id = $1",
    )
    .bind(comms_request_id)
    .fetch_all(&tenant.tenant_pool)
    .await
    .expect("fetching comms_event rows failed");
    assert_eq!(
        events,
        vec![(
            "sent".to_string(),
            Some(format!("msg-sent-{channel}-1")),
            Some("queued".to_string())
        )]
    );

    let _ = customer_id;
    tenant.cleanup().await;
}

#[tokio::test]
async fn successful_send_for_email_channel_writes_sent_event_and_final_status() {
    assert_successful_send_for_channel("email", "jordan@example.com").await;
}

#[tokio::test]
async fn successful_send_for_whatsapp_channel_writes_sent_event_and_final_status() {
    assert_successful_send_for_channel("whatsapp", "+15550199").await;
}

#[tokio::test]
async fn successful_send_writes_sent_event_and_final_status_and_deletes_the_outbox_row()
{
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "msg-sent-1",
            "status": "queued",
        })))
        .mount(&mock_server)
        .await;

    let (comms_request_id, created_at, customer_id) =
        write_ready_outbox_row(&tenant, &vault, &cache, "+15550100", "hello there")
            .await;

    let sender: Arc<dyn Sender> =
        Arc::new(HttpSender::new(mock_server.uri(), "test-key".to_string()));
    let ctx = DispatcherContext {
        pool: tenant.tenant_pool.clone(),
        keystore: Arc::new(vault_keystore()),
        cache: Arc::new(cache),
        mount: tenant.mount.clone(),
        sender,
        verification_mode: "observe".to_string(),
        default_timezone: "UTC".to_string(),
        quiet_hours_policy: None,
        kill_switches: Arc::new(KillSwitchCache::new()),
        draining: Arc::new(RwLock::new(HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(QuotaTracker::new("UTC")),
    };

    let claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].comms_request_id, comms_request_id);

    try_process(&ctx, &claimed[0])
        .await
        .expect("try_process failed");

    let final_status: Option<String> = sqlx::query_scalar(
        "SELECT final_status FROM comms_request WHERE created_at = $1 AND id = $2",
    )
    .bind(created_at)
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching final_status failed");
    assert_eq!(final_status.as_deref(), Some("sent"));

    let events: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT event_type, provider_ref, provider_status FROM comms_event WHERE comms_request_id = $1",
    )
    .bind(comms_request_id)
    .fetch_all(&tenant.tenant_pool)
    .await
    .expect("fetching comms_event rows failed");
    assert_eq!(
        events,
        vec![(
            "sent".to_string(),
            Some("msg-sent-1".to_string()),
            Some("queued".to_string())
        )]
    );

    let outbox_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox WHERE comms_request_id = $1")
            .bind(comms_request_id)
            .fetch_one(&tenant.tenant_pool)
            .await
            .expect("counting outbox rows failed");
    assert_eq!(
        outbox_count, 0,
        "a terminal row must be removed from the queue"
    );

    let _ = customer_id;
    tenant.cleanup().await;
}

#[tokio::test]
async fn terminal_provider_rejection_writes_failed_event_and_final_status_with_no_requeue()
 {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad destination"))
        .mount(&mock_server)
        .await;

    let (comms_request_id, created_at, _customer_id) =
        write_ready_outbox_row(&tenant, &vault, &cache, "+15550100", "hello there")
            .await;

    let sender: Arc<dyn Sender> =
        Arc::new(HttpSender::new(mock_server.uri(), "test-key".to_string()));
    let ctx = DispatcherContext {
        pool: tenant.tenant_pool.clone(),
        keystore: Arc::new(vault_keystore()),
        cache: Arc::new(cache),
        mount: tenant.mount.clone(),
        sender,
        verification_mode: "observe".to_string(),
        default_timezone: "UTC".to_string(),
        quiet_hours_policy: None,
        kill_switches: Arc::new(KillSwitchCache::new()),
        draining: Arc::new(RwLock::new(HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(QuotaTracker::new("UTC")),
    };

    let claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed.len(), 1);

    try_process(&ctx, &claimed[0])
        .await
        .expect("try_process failed");

    let final_status: Option<String> = sqlx::query_scalar(
        "SELECT final_status FROM comms_request WHERE created_at = $1 AND id = $2",
    )
    .bind(created_at)
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching final_status failed");
    assert_eq!(final_status.as_deref(), Some("failed"));

    let events: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT event_type, provider_ref, provider_status FROM comms_event WHERE comms_request_id = $1",
    )
    .bind(comms_request_id)
    .fetch_all(&tenant.tenant_pool)
    .await
    .expect("fetching comms_event rows failed");
    assert_eq!(
        events,
        vec![(
            "failed".to_string(),
            Some(String::new()),
            Some("400".to_string())
        )]
    );

    let outbox_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox WHERE comms_request_id = $1")
            .bind(comms_request_id)
            .fetch_one(&tenant.tenant_pool)
            .await
            .expect("counting outbox rows failed");
    assert_eq!(
        outbox_count, 0,
        "a terminal (4xx-equivalent) rejection must be removed from the queue, not requeued"
    );

    tenant.cleanup().await;
}

#[tokio::test]
async fn transient_provider_failure_requeues_with_cleared_lease_and_backoff() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(
            ResponseTemplate::new(500).set_body_string("provider unavailable"),
        )
        .mount(&mock_server)
        .await;

    let (comms_request_id, created_at, _customer_id) =
        write_ready_outbox_row(&tenant, &vault, &cache, "+15550100", "hello there")
            .await;

    let sender: Arc<dyn Sender> =
        Arc::new(HttpSender::new(mock_server.uri(), "test-key".to_string()));
    let ctx = DispatcherContext {
        pool: tenant.tenant_pool.clone(),
        keystore: Arc::new(vault_keystore()),
        cache: Arc::new(cache),
        mount: tenant.mount.clone(),
        sender,
        verification_mode: "observe".to_string(),
        default_timezone: "UTC".to_string(),
        quiet_hours_policy: None,
        kill_switches: Arc::new(KillSwitchCache::new()),
        draining: Arc::new(RwLock::new(HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(QuotaTracker::new("UTC")),
    };

    let claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed.len(), 1);

    try_process(&ctx, &claimed[0])
        .await
        .expect("try_process failed");

    let final_status: Option<String> = sqlx::query_scalar(
        "SELECT final_status FROM comms_request WHERE created_at = $1 AND id = $2",
    )
    .bind(created_at)
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching final_status failed");
    assert_eq!(
        final_status, None,
        "a retryable failure must not finalize the ledger row"
    );

    let event_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM comms_event WHERE comms_request_id = $1",
    )
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("counting comms_event rows failed");
    assert_eq!(
        event_count, 0,
        "a reschedule writes no comms_event row (T-021 decision 3)"
    );

    let (leased_until, next_attempt_at): (Option<DateTime<Utc>>, DateTime<Utc>) = sqlx::query_as(
        "SELECT leased_until, next_attempt_at FROM outbox WHERE comms_request_id = $1",
    )
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching outbox row failed");
    assert_eq!(
        leased_until, None,
        "a retryable failure must clear the lease in the same statement as the reschedule"
    );
    assert!(
        next_attempt_at > Utc::now(),
        "a retryable failure must reschedule into the future"
    );

    tenant.cleanup().await;
}

#[tokio::test]
async fn transient_http_failure_requeues() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let (comms_request_id, _created_at, _customer_id) =
        write_ready_outbox_row(&tenant, &vault, &cache, "+15550100", "hello there")
            .await;

    // An unreachable base URL fails at the transport layer, before any HTTP
    // response exists -- SenderError::Http, not SenderError::Provider.
    let sender: Arc<dyn Sender> = Arc::new(HttpSender::new(
        "http://127.0.0.1:1".to_string(),
        "test-key".to_string(),
    ));
    let ctx = DispatcherContext {
        pool: tenant.tenant_pool.clone(),
        keystore: Arc::new(vault_keystore()),
        cache: Arc::new(cache),
        mount: tenant.mount.clone(),
        sender,
        verification_mode: "observe".to_string(),
        default_timezone: "UTC".to_string(),
        quiet_hours_policy: None,
        kill_switches: Arc::new(KillSwitchCache::new()),
        draining: Arc::new(RwLock::new(HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(QuotaTracker::new("UTC")),
    };

    let claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed.len(), 1);

    try_process(&ctx, &claimed[0])
        .await
        .expect("try_process failed");

    let (leased_until, next_attempt_at): (Option<DateTime<Utc>>, DateTime<Utc>) = sqlx::query_as(
        "SELECT leased_until, next_attempt_at FROM outbox WHERE comms_request_id = $1",
    )
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching outbox row failed");
    assert_eq!(
        leased_until, None,
        "a transport-level failure must clear the lease in the same statement as the reschedule"
    );
    assert!(
        next_attempt_at > Utc::now(),
        "a transport-level failure must reschedule into the future"
    );

    tenant.cleanup().await;
}

#[tokio::test]
async fn retries_exhausted_after_max_attempts_terminal_fails() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(
            ResponseTemplate::new(500).set_body_string("provider unavailable"),
        )
        .mount(&mock_server)
        .await;

    let (comms_request_id, created_at, _customer_id) =
        write_ready_outbox_row(&tenant, &vault, &cache, "+15550100", "hello there")
            .await;

    let sender: Arc<dyn Sender> =
        Arc::new(HttpSender::new(mock_server.uri(), "test-key".to_string()));
    let ctx = DispatcherContext {
        pool: tenant.tenant_pool.clone(),
        keystore: Arc::new(vault_keystore()),
        cache: Arc::new(cache),
        mount: tenant.mount.clone(),
        sender,
        verification_mode: "observe".to_string(),
        default_timezone: "UTC".to_string(),
        quiet_hours_policy: None,
        kill_switches: Arc::new(KillSwitchCache::new()),
        draining: Arc::new(RwLock::new(HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(QuotaTracker::new("UTC")),
    };

    let mut claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed.len(), 1);

    // Simulate the 8th claim (MAX_SEND_ATTEMPTS) without actually retrying
    // seven times -- claim's own UPDATE already bumped attempts to 1.
    sqlx::query("UPDATE outbox SET attempts = 8 WHERE comms_request_id = $1")
        .bind(comms_request_id)
        .execute(&tenant.tenant_pool)
        .await
        .expect("bumping attempts failed");
    claimed[0].attempts = 8;

    try_process(&ctx, &claimed[0])
        .await
        .expect("try_process failed");

    let final_status: Option<String> = sqlx::query_scalar(
        "SELECT final_status FROM comms_request WHERE created_at = $1 AND id = $2",
    )
    .bind(created_at)
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching final_status failed");
    assert_eq!(
        final_status.as_deref(),
        Some("failed"),
        "exhausted retries must terminal-fail rather than reschedule again"
    );

    let events: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT event_type, provider_ref, provider_status FROM comms_event WHERE comms_request_id = $1",
    )
    .bind(comms_request_id)
    .fetch_all(&tenant.tenant_pool)
    .await
    .expect("fetching comms_event rows failed");
    assert_eq!(
        events,
        vec![(
            "failed".to_string(),
            Some(String::new()),
            Some("500".to_string())
        )]
    );

    let outbox_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox WHERE comms_request_id = $1")
            .bind(comms_request_id)
            .fetch_one(&tenant.tenant_pool)
            .await
            .expect("counting outbox rows failed");
    assert_eq!(
        outbox_count, 0,
        "an exhausted row must be removed from the queue"
    );

    tenant.cleanup().await;
}

#[tokio::test]
async fn seventh_attempt_still_reschedules_one_short_of_the_cap() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(
            ResponseTemplate::new(500).set_body_string("provider unavailable"),
        )
        .mount(&mock_server)
        .await;

    let (comms_request_id, created_at, _customer_id) =
        write_ready_outbox_row(&tenant, &vault, &cache, "+15550100", "hello there")
            .await;

    let sender: Arc<dyn Sender> =
        Arc::new(HttpSender::new(mock_server.uri(), "test-key".to_string()));
    let ctx = DispatcherContext {
        pool: tenant.tenant_pool.clone(),
        keystore: Arc::new(vault_keystore()),
        cache: Arc::new(cache),
        mount: tenant.mount.clone(),
        sender,
        verification_mode: "observe".to_string(),
        default_timezone: "UTC".to_string(),
        quiet_hours_policy: None,
        kill_switches: Arc::new(KillSwitchCache::new()),
        draining: Arc::new(RwLock::new(HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(QuotaTracker::new("UTC")),
    };

    let mut claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed.len(), 1);

    // One short of MAX_SEND_ATTEMPTS (8) -- the guard `row.attempts <
    // MAX_SEND_ATTEMPTS` must still admit this and reschedule, not
    // terminal-fail. Catches an off-by-one that terminal-fails a beat early.
    sqlx::query("UPDATE outbox SET attempts = 7 WHERE comms_request_id = $1")
        .bind(comms_request_id)
        .execute(&tenant.tenant_pool)
        .await
        .expect("bumping attempts failed");
    claimed[0].attempts = 7;

    try_process(&ctx, &claimed[0])
        .await
        .expect("try_process failed");

    let final_status: Option<String> = sqlx::query_scalar(
        "SELECT final_status FROM comms_request WHERE created_at = $1 AND id = $2",
    )
    .bind(created_at)
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching final_status failed");
    assert_eq!(
        final_status, None,
        "attempts == 7 (one short of the cap) must still reschedule, not terminal-fail"
    );

    let (leased_until, next_attempt_at): (Option<DateTime<Utc>>, DateTime<Utc>) = sqlx::query_as(
        "SELECT leased_until, next_attempt_at FROM outbox WHERE comms_request_id = $1",
    )
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching outbox row failed");
    assert_eq!(leased_until, None);
    assert!(next_attempt_at > Utc::now());

    tenant.cleanup().await;
}

#[tokio::test]
async fn attempts_past_the_cap_still_terminal_fails() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(
            ResponseTemplate::new(500).set_body_string("provider unavailable"),
        )
        .mount(&mock_server)
        .await;

    let (comms_request_id, created_at, _customer_id) =
        write_ready_outbox_row(&tenant, &vault, &cache, "+15550100", "hello there")
            .await;

    let sender: Arc<dyn Sender> =
        Arc::new(HttpSender::new(mock_server.uri(), "test-key".to_string()));
    let ctx = DispatcherContext {
        pool: tenant.tenant_pool.clone(),
        keystore: Arc::new(vault_keystore()),
        cache: Arc::new(cache),
        mount: tenant.mount.clone(),
        sender,
        verification_mode: "observe".to_string(),
        default_timezone: "UTC".to_string(),
        quiet_hours_policy: None,
        kill_switches: Arc::new(KillSwitchCache::new()),
        draining: Arc::new(RwLock::new(HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(QuotaTracker::new("UTC")),
    };

    let mut claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed.len(), 1);

    // Past the cap -- should never happen in practice (nothing reschedules
    // past 8), but the guard must still terminal-fail rather than reschedule
    // forever if it ever does.
    sqlx::query("UPDATE outbox SET attempts = 9 WHERE comms_request_id = $1")
        .bind(comms_request_id)
        .execute(&tenant.tenant_pool)
        .await
        .expect("bumping attempts failed");
    claimed[0].attempts = 9;

    try_process(&ctx, &claimed[0])
        .await
        .expect("try_process failed");

    let final_status: Option<String> = sqlx::query_scalar(
        "SELECT final_status FROM comms_request WHERE created_at = $1 AND id = $2",
    )
    .bind(created_at)
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching final_status failed");
    assert_eq!(final_status.as_deref(), Some("failed"));

    tenant.cleanup().await;
}

#[tokio::test]
async fn startup_sweep_reclaims_a_lease_left_by_a_simulated_crash() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    write_ready_outbox_row(&tenant, &vault, &cache, "+15550100", "hello there").await;

    // Claim it and go no further -- simulates a crash mid try_process,
    // before any terminal write or reschedule ever ran.
    let claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed.len(), 1);
    let leased_id = claimed[0].comms_request_id;

    // Without the sweep, this row would stay leased forever -- repo::claim's
    // own predicate never compares leased_until to now() (T-021's own
    // corrected framing).
    let second_claim_before_sweep = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("second claim failed");
    assert!(
        second_claim_before_sweep.is_empty(),
        "a leased row must not be claimable before the sweep runs"
    );

    let cleared = repo::clear_stale_leases(&tenant.tenant_pool)
        .await
        .expect("clear_stale_leases failed");
    assert_eq!(cleared, 1);

    let reclaimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("reclaim after sweep failed");
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(
        reclaimed[0].comms_request_id, leased_id,
        "the sweep must make the exact same row claimable again, immediately"
    );

    tenant.cleanup().await;
}

#[tokio::test]
async fn concurrent_claims_never_double_claim_the_same_row() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    write_ready_outbox_row(&tenant, &vault, &cache, "+15550100", "row one").await;
    write_ready_outbox_row(&tenant, &vault, &cache, "+15550101", "row two").await;

    let leased_until = Utc::now() + chrono::Duration::minutes(2);
    let exclusion = no_exclusion();
    let (first, second) = tokio::join!(
        repo::claim(&tenant.tenant_pool, "sms", 1, leased_until, &exclusion),
        repo::claim(&tenant.tenant_pool, "sms", 1, leased_until, &exclusion),
    );
    let first: Vec<ClaimedOutbox> = first.expect("first claim failed");
    let second: Vec<ClaimedOutbox> = second.expect("second claim failed");

    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_ne!(
        first[0].comms_request_id, second[0].comms_request_id,
        "SKIP LOCKED must prevent two concurrent claims from returning the same row"
    );

    tenant.cleanup().await;
}

#[tokio::test]
async fn claim_ignores_leased_and_not_yet_due_rows() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let (ready_id, _, _) =
        write_ready_outbox_row(&tenant, &vault, &cache, "+15550100", "ready row").await;
    let (future_id, _, _) =
        write_ready_outbox_row(&tenant, &vault, &cache, "+15550101", "future row")
            .await;

    sqlx::query("UPDATE outbox SET next_attempt_at = now() + interval '1 hour' WHERE comms_request_id = $1")
        .bind(future_id)
        .execute(&tenant.tenant_pool)
        .await
        .expect("pushing next_attempt_at forward failed");

    let claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].comms_request_id, ready_id);

    // The already-claimed row is now leased; a second claim must not pick it
    // (or the future-dated row) back up.
    let second = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("second claim failed");
    assert!(second.is_empty());

    tenant.cleanup().await;
}

#[tokio::test]
async fn inserting_an_outbox_row_notifies_the_channels_listener() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let mut listener = sqlx::postgres::PgListener::connect_with(&tenant.tenant_pool)
        .await
        .expect("opening a LISTEN connection failed");
    listener
        .listen("outbox_sms")
        .await
        .expect("LISTEN outbox_sms failed");

    let (comms_request_id, _, _) =
        write_ready_outbox_row(&tenant, &vault, &cache, "+15550100", "notify me").await;

    let notification = tokio::time::timeout(Duration::from_secs(5), listener.recv())
        .await
        .expect("timed out waiting for a notification")
        .expect("receiving a notification failed");

    assert_eq!(notification.channel(), "outbox_sms");
    assert_eq!(notification.payload(), comms_request_id.to_string());

    tenant.cleanup().await;
}

// --- T-036: verification gate ---

#[tokio::test]
async fn enforce_blocks_unverified_address_with_terminal_event_and_no_send() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "should-not-be-called",
            "status": "queued",
        })))
        .expect(0)
        .mount(&mock_server)
        .await;

    let (comms_request_id, created_at, _customer_id) =
        write_outbox_row_with_verification(
            &tenant,
            &vault,
            &cache,
            "+15550100",
            "hello there",
            "transactional",
            None,
            b"unused-hmac",
            None,
            Uuid::new_v4(),
        )
        .await;

    let sender: Arc<dyn Sender> =
        Arc::new(HttpSender::new(mock_server.uri(), "test-key".to_string()));
    let ctx = DispatcherContext {
        pool: tenant.tenant_pool.clone(),
        keystore: Arc::new(vault_keystore()),
        cache: Arc::new(cache),
        mount: tenant.mount.clone(),
        sender,
        verification_mode: "enforce".to_string(),
        default_timezone: "UTC".to_string(),
        quiet_hours_policy: None,
        kill_switches: Arc::new(KillSwitchCache::new()),
        draining: Arc::new(RwLock::new(HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(QuotaTracker::new("UTC")),
    };

    let claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed.len(), 1);

    try_process(&ctx, &claimed[0])
        .await
        .expect("try_process failed");

    let final_status: Option<String> = sqlx::query_scalar(
        "SELECT final_status FROM comms_request WHERE created_at = $1 AND id = $2",
    )
    .bind(created_at)
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching final_status failed");
    assert_eq!(final_status.as_deref(), Some("unverified_address"));

    let events: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT event_type, provider_ref, provider_status FROM comms_event \
         WHERE comms_request_id = $1 ORDER BY occurred_at",
    )
    .bind(comms_request_id)
    .fetch_all(&tenant.tenant_pool)
    .await
    .expect("fetching comms_event rows failed");
    assert_eq!(
        events,
        vec![("unverified_address".to_string(), Some(String::new()), None)]
    );

    let outbox_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox WHERE comms_request_id = $1")
            .bind(comms_request_id)
            .fetch_one(&tenant.tenant_pool)
            .await
            .expect("counting outbox rows failed");
    assert_eq!(
        outbox_count, 0,
        "a terminal row must be removed from the queue"
    );

    tenant.cleanup().await;
}

#[tokio::test]
async fn observe_records_unverified_address_and_still_sends() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "msg-observe-1",
            "status": "queued",
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    let (comms_request_id, created_at, _customer_id) =
        write_outbox_row_with_verification(
            &tenant,
            &vault,
            &cache,
            "+15550100",
            "hello there",
            "transactional",
            None,
            b"unused-hmac",
            None,
            Uuid::new_v4(),
        )
        .await;

    let sender: Arc<dyn Sender> =
        Arc::new(HttpSender::new(mock_server.uri(), "test-key".to_string()));
    let ctx = DispatcherContext {
        pool: tenant.tenant_pool.clone(),
        keystore: Arc::new(vault_keystore()),
        cache: Arc::new(cache),
        mount: tenant.mount.clone(),
        sender,
        verification_mode: "observe".to_string(),
        default_timezone: "UTC".to_string(),
        quiet_hours_policy: None,
        kill_switches: Arc::new(KillSwitchCache::new()),
        draining: Arc::new(RwLock::new(HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(QuotaTracker::new("UTC")),
    };

    let claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed.len(), 1);

    try_process(&ctx, &claimed[0])
        .await
        .expect("try_process failed");

    let final_status: Option<String> = sqlx::query_scalar(
        "SELECT final_status FROM comms_request WHERE created_at = $1 AND id = $2",
    )
    .bind(created_at)
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching final_status failed");
    assert_eq!(final_status.as_deref(), Some("sent"));

    let events: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT event_type, provider_ref, provider_status FROM comms_event \
         WHERE comms_request_id = $1 ORDER BY occurred_at",
    )
    .bind(comms_request_id)
    .fetch_all(&tenant.tenant_pool)
    .await
    .expect("fetching comms_event rows failed");
    assert_eq!(
        events,
        vec![
            ("unverified_address".to_string(), Some(String::new()), None),
            (
                "sent".to_string(),
                Some("msg-observe-1".to_string()),
                Some("queued".to_string())
            ),
        ],
        "observe must record the outcome and still let the send through"
    );

    let outbox_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox WHERE comms_request_id = $1")
            .bind(comms_request_id)
            .fetch_one(&tenant.tenant_pool)
            .await
            .expect("counting outbox rows failed");
    assert_eq!(outbox_count, 0);

    tenant.cleanup().await;
}

#[tokio::test]
async fn verified_address_sends_normally_under_enforce() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "msg-verified-1",
            "status": "queued",
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    let (comms_request_id, created_at, _customer_id) =
        write_outbox_row_with_verification(
            &tenant,
            &vault,
            &cache,
            "+15550100",
            "hello there",
            "transactional",
            Some(Utc::now()),
            b"unused-hmac",
            None,
            Uuid::new_v4(),
        )
        .await;

    let sender: Arc<dyn Sender> =
        Arc::new(HttpSender::new(mock_server.uri(), "test-key".to_string()));
    let ctx = DispatcherContext {
        pool: tenant.tenant_pool.clone(),
        keystore: Arc::new(vault_keystore()),
        cache: Arc::new(cache),
        mount: tenant.mount.clone(),
        sender,
        verification_mode: "enforce".to_string(),
        default_timezone: "UTC".to_string(),
        quiet_hours_policy: None,
        kill_switches: Arc::new(KillSwitchCache::new()),
        draining: Arc::new(RwLock::new(HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(QuotaTracker::new("UTC")),
    };

    let claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed.len(), 1);

    try_process(&ctx, &claimed[0])
        .await
        .expect("try_process failed");

    let final_status: Option<String> = sqlx::query_scalar(
        "SELECT final_status FROM comms_request WHERE created_at = $1 AND id = $2",
    )
    .bind(created_at)
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching final_status failed");
    assert_eq!(final_status.as_deref(), Some("sent"));

    let events: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT event_type, provider_ref, provider_status FROM comms_event WHERE comms_request_id = $1",
    )
    .bind(comms_request_id)
    .fetch_all(&tenant.tenant_pool)
    .await
    .expect("fetching comms_event rows failed");
    assert_eq!(
        events,
        vec![(
            "sent".to_string(),
            Some("msg-verified-1".to_string()),
            Some("queued".to_string())
        )],
        "enforce must not block a verified address"
    );

    tenant.cleanup().await;
}

#[tokio::test]
async fn auth_class_skips_the_gate_even_when_unverified() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "msg-auth-1",
            "status": "queued",
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    let (comms_request_id, created_at, _customer_id) =
        write_outbox_row_with_verification(
            &tenant,
            &vault,
            &cache,
            "+15550100",
            "your code is 123456",
            "auth",
            None,
            b"unused-hmac",
            None,
            Uuid::new_v4(),
        )
        .await;

    let sender: Arc<dyn Sender> =
        Arc::new(HttpSender::new(mock_server.uri(), "test-key".to_string()));
    let ctx = DispatcherContext {
        pool: tenant.tenant_pool.clone(),
        keystore: Arc::new(vault_keystore()),
        cache: Arc::new(cache),
        mount: tenant.mount.clone(),
        sender,
        verification_mode: "enforce".to_string(),
        default_timezone: "UTC".to_string(),
        quiet_hours_policy: None,
        kill_switches: Arc::new(KillSwitchCache::new()),
        draining: Arc::new(RwLock::new(HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(QuotaTracker::new("UTC")),
    };

    let claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed.len(), 1);

    try_process(&ctx, &claimed[0])
        .await
        .expect("try_process failed");

    let final_status: Option<String> = sqlx::query_scalar(
        "SELECT final_status FROM comms_request WHERE created_at = $1 AND id = $2",
    )
    .bind(created_at)
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching final_status failed");
    assert_eq!(final_status.as_deref(), Some("sent"));

    let events: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT event_type, provider_ref, provider_status FROM comms_event WHERE comms_request_id = $1",
    )
    .bind(comms_request_id)
    .fetch_all(&tenant.tenant_pool)
    .await
    .expect("fetching comms_event rows failed");
    assert_eq!(
        events,
        vec![(
            "sent".to_string(),
            Some("msg-auth-1".to_string()),
            Some("queued".to_string())
        )],
        "auth class must skip the verification gate entirely, even unverified under enforce"
    );

    tenant.cleanup().await;
}

async fn insert_suppression_row(
    pool: &PgPool,
    destination_hmac: &[u8],
    review_at: DateTime<Utc>,
) {
    sqlx::query(
        "INSERT INTO suppression (destination_hmac, reason, added_at, review_at) \
         VALUES ($1, 'hard_bounce', now(), $2)",
    )
    .bind(destination_hmac)
    .bind(review_at)
    .execute(pool)
    .await
    .expect("inserting suppression row failed");
}

#[tokio::test]
async fn an_active_suppression_entry_blocks_the_send() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "msg-should-not-send",
            "status": "queued",
        })))
        .expect(0)
        .mount(&mock_server)
        .await;

    insert_suppression_row(
        &tenant.tenant_pool,
        b"suppressed-address",
        Utc::now() + chrono::Duration::days(1),
    )
    .await;

    let (comms_request_id, created_at, _customer_id) = write_outbox_row_with_hmac(
        &tenant,
        &vault,
        &cache,
        "+15550100",
        "hello there",
        b"suppressed-address",
    )
    .await;

    let sender: Arc<dyn Sender> =
        Arc::new(HttpSender::new(mock_server.uri(), "test-key".to_string()));
    let ctx = DispatcherContext {
        pool: tenant.tenant_pool.clone(),
        keystore: Arc::new(vault_keystore()),
        cache: Arc::new(cache),
        mount: tenant.mount.clone(),
        sender,
        verification_mode: "observe".to_string(),
        default_timezone: "UTC".to_string(),
        quiet_hours_policy: None,
        kill_switches: Arc::new(KillSwitchCache::new()),
        draining: Arc::new(RwLock::new(HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(QuotaTracker::new("UTC")),
    };

    let claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].comms_request_id, comms_request_id);

    try_process(&ctx, &claimed[0])
        .await
        .expect("try_process failed");

    let final_status: Option<String> = sqlx::query_scalar(
        "SELECT final_status FROM comms_request WHERE created_at = $1 AND id = $2",
    )
    .bind(created_at)
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching final_status failed");
    assert_eq!(final_status.as_deref(), Some("suppressed_list"));

    let events: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT event_type, provider_ref, provider_status FROM comms_event WHERE comms_request_id = $1",
    )
    .bind(comms_request_id)
    .fetch_all(&tenant.tenant_pool)
    .await
    .expect("fetching comms_event rows failed");
    assert_eq!(
        events,
        vec![("suppressed_list".to_string(), Some(String::new()), None)]
    );

    let outbox_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox WHERE comms_request_id = $1")
            .bind(comms_request_id)
            .fetch_one(&tenant.tenant_pool)
            .await
            .expect("counting outbox rows failed");
    assert_eq!(outbox_count, 0);

    tenant.cleanup().await;
}

#[tokio::test]
async fn an_expired_suppression_entry_no_longer_blocks() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "msg-sent-after-expiry",
            "status": "queued",
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    insert_suppression_row(
        &tenant.tenant_pool,
        b"suppressed-address",
        Utc::now() - chrono::Duration::hours(1),
    )
    .await;

    let (comms_request_id, created_at, _customer_id) = write_outbox_row_with_hmac(
        &tenant,
        &vault,
        &cache,
        "+15550100",
        "hello there",
        b"suppressed-address",
    )
    .await;

    let sender: Arc<dyn Sender> =
        Arc::new(HttpSender::new(mock_server.uri(), "test-key".to_string()));
    let ctx = DispatcherContext {
        pool: tenant.tenant_pool.clone(),
        keystore: Arc::new(vault_keystore()),
        cache: Arc::new(cache),
        mount: tenant.mount.clone(),
        sender,
        verification_mode: "observe".to_string(),
        default_timezone: "UTC".to_string(),
        quiet_hours_policy: None,
        kill_switches: Arc::new(KillSwitchCache::new()),
        draining: Arc::new(RwLock::new(HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(QuotaTracker::new("UTC")),
    };

    let claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].comms_request_id, comms_request_id);

    try_process(&ctx, &claimed[0])
        .await
        .expect("try_process failed");

    let final_status: Option<String> = sqlx::query_scalar(
        "SELECT final_status FROM comms_request WHERE created_at = $1 AND id = $2",
    )
    .bind(created_at)
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching final_status failed");
    assert_eq!(final_status.as_deref(), Some("sent"));

    let outbox_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox WHERE comms_request_id = $1")
            .bind(comms_request_id)
            .fetch_one(&tenant.tenant_pool)
            .await
            .expect("counting outbox rows failed");
    assert_eq!(outbox_count, 0);

    tenant.cleanup().await;
}

#[tokio::test]
async fn expired_row_is_terminal_written_and_not_sent() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "should-not-be-called",
            "status": "queued",
        })))
        .expect(0)
        .mount(&mock_server)
        .await;

    let (comms_request_id, created_at, _customer_id) =
        write_outbox_row_with_verification(
            &tenant,
            &vault,
            &cache,
            "+15550100",
            "hello there",
            "transactional",
            Some(Utc::now()),
            b"unused-hmac",
            Some(Utc::now() - chrono::Duration::minutes(5)),
            Uuid::new_v4(),
        )
        .await;

    let sender: Arc<dyn Sender> =
        Arc::new(HttpSender::new(mock_server.uri(), "test-key".to_string()));
    let ctx = DispatcherContext {
        pool: tenant.tenant_pool.clone(),
        keystore: Arc::new(vault_keystore()),
        cache: Arc::new(cache),
        mount: tenant.mount.clone(),
        sender,
        verification_mode: "enforce".to_string(),
        default_timezone: "UTC".to_string(),
        quiet_hours_policy: None,
        kill_switches: Arc::new(KillSwitchCache::new()),
        draining: Arc::new(RwLock::new(HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(QuotaTracker::new("UTC")),
    };

    let claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed.len(), 1);

    try_process(&ctx, &claimed[0])
        .await
        .expect("try_process failed");

    let final_status: Option<String> = sqlx::query_scalar(
        "SELECT final_status FROM comms_request WHERE created_at = $1 AND id = $2",
    )
    .bind(created_at)
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching final_status failed");
    assert_eq!(final_status.as_deref(), Some("expired"));

    let outbox_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox WHERE comms_request_id = $1")
            .bind(comms_request_id)
            .fetch_one(&tenant.tenant_pool)
            .await
            .expect("counting outbox rows failed");
    assert_eq!(
        outbox_count, 0,
        "a terminal row must be removed from the queue"
    );

    tenant.cleanup().await;
}

async fn insert_consent_row(
    pool: &PgPool,
    address_id: Uuid,
    class: &str,
    opted_in: bool,
) {
    sqlx::query(
        "INSERT INTO consent (address_id, class, opted_in, source, updated_at) \
         VALUES ($1, $2, $3, 'test', now())",
    )
    .bind(address_id)
    .bind(class)
    .bind(opted_in)
    .execute(pool)
    .await
    .expect("inserting consent row failed");
}

#[tokio::test]
async fn marketing_without_any_consent_row_is_blocked_with_suppressed_consent() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "msg-should-not-send",
            "status": "queued",
        })))
        .expect(0)
        .mount(&mock_server)
        .await;

    let (comms_request_id, created_at, _customer_id) =
        write_outbox_row_with_verification(
            &tenant,
            &vault,
            &cache,
            "+15550100",
            "hello there",
            "marketing",
            Some(Utc::now()),
            unique_name("hmac").as_bytes(),
            None,
            Uuid::new_v4(),
        )
        .await;

    let sender: Arc<dyn Sender> =
        Arc::new(HttpSender::new(mock_server.uri(), "test-key".to_string()));
    let ctx = DispatcherContext {
        pool: tenant.tenant_pool.clone(),
        keystore: Arc::new(vault_keystore()),
        cache: Arc::new(cache),
        mount: tenant.mount.clone(),
        sender,
        verification_mode: "observe".to_string(),
        default_timezone: "UTC".to_string(),
        quiet_hours_policy: None,
        kill_switches: Arc::new(KillSwitchCache::new()),
        draining: Arc::new(RwLock::new(HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(QuotaTracker::new("UTC")),
    };

    let claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].comms_request_id, comms_request_id);

    try_process(&ctx, &claimed[0])
        .await
        .expect("try_process failed");

    let final_status: Option<String> = sqlx::query_scalar(
        "SELECT final_status FROM comms_request WHERE created_at = $1 AND id = $2",
    )
    .bind(created_at)
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching final_status failed");
    assert_eq!(final_status.as_deref(), Some("suppressed_consent"));

    let events: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT event_type, provider_ref, provider_status FROM comms_event WHERE comms_request_id = $1",
    )
    .bind(comms_request_id)
    .fetch_all(&tenant.tenant_pool)
    .await
    .expect("fetching comms_event rows failed");
    assert_eq!(
        events,
        vec![("suppressed_consent".to_string(), Some(String::new()), None)]
    );

    let outbox_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox WHERE comms_request_id = $1")
            .bind(comms_request_id)
            .fetch_one(&tenant.tenant_pool)
            .await
            .expect("counting outbox rows failed");
    assert_eq!(outbox_count, 0);

    tenant.cleanup().await;
}

#[tokio::test]
async fn marketing_with_an_explicit_opt_out_is_blocked() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "msg-should-not-send",
            "status": "queued",
        })))
        .expect(0)
        .mount(&mock_server)
        .await;

    let (comms_request_id, created_at, _customer_id) =
        write_outbox_row_with_verification(
            &tenant,
            &vault,
            &cache,
            "+15550100",
            "hello there",
            "marketing",
            Some(Utc::now()),
            unique_name("hmac").as_bytes(),
            None,
            Uuid::new_v4(),
        )
        .await;

    let sender: Arc<dyn Sender> =
        Arc::new(HttpSender::new(mock_server.uri(), "test-key".to_string()));
    let ctx = DispatcherContext {
        pool: tenant.tenant_pool.clone(),
        keystore: Arc::new(vault_keystore()),
        cache: Arc::new(cache),
        mount: tenant.mount.clone(),
        sender,
        verification_mode: "observe".to_string(),
        default_timezone: "UTC".to_string(),
        quiet_hours_policy: None,
        kill_switches: Arc::new(KillSwitchCache::new()),
        draining: Arc::new(RwLock::new(HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(QuotaTracker::new("UTC")),
    };

    let claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].comms_request_id, comms_request_id);

    insert_consent_row(
        &tenant.tenant_pool,
        claimed[0].address_id,
        "marketing",
        false,
    )
    .await;

    try_process(&ctx, &claimed[0])
        .await
        .expect("try_process failed");

    let final_status: Option<String> = sqlx::query_scalar(
        "SELECT final_status FROM comms_request WHERE created_at = $1 AND id = $2",
    )
    .bind(created_at)
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching final_status failed");
    assert_eq!(final_status.as_deref(), Some("suppressed_consent"));

    let events: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT event_type, provider_ref, provider_status FROM comms_event WHERE comms_request_id = $1",
    )
    .bind(comms_request_id)
    .fetch_all(&tenant.tenant_pool)
    .await
    .expect("fetching comms_event rows failed");
    assert_eq!(
        events,
        vec![("suppressed_consent".to_string(), Some(String::new()), None)]
    );

    let outbox_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox WHERE comms_request_id = $1")
            .bind(comms_request_id)
            .fetch_one(&tenant.tenant_pool)
            .await
            .expect("counting outbox rows failed");
    assert_eq!(outbox_count, 0);

    tenant.cleanup().await;
}

#[tokio::test]
async fn marketing_with_an_explicit_opt_in_sends() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "msg-opted-in",
            "status": "queued",
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    let (comms_request_id, created_at, _customer_id) =
        write_outbox_row_with_verification(
            &tenant,
            &vault,
            &cache,
            "+15550100",
            "hello there",
            "marketing",
            Some(Utc::now()),
            unique_name("hmac").as_bytes(),
            None,
            Uuid::new_v4(),
        )
        .await;

    let sender: Arc<dyn Sender> =
        Arc::new(HttpSender::new(mock_server.uri(), "test-key".to_string()));
    let ctx = DispatcherContext {
        pool: tenant.tenant_pool.clone(),
        keystore: Arc::new(vault_keystore()),
        cache: Arc::new(cache),
        mount: tenant.mount.clone(),
        sender,
        verification_mode: "observe".to_string(),
        default_timezone: "UTC".to_string(),
        quiet_hours_policy: None,
        kill_switches: Arc::new(KillSwitchCache::new()),
        draining: Arc::new(RwLock::new(HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(QuotaTracker::new("UTC")),
    };

    let claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].comms_request_id, comms_request_id);

    insert_consent_row(
        &tenant.tenant_pool,
        claimed[0].address_id,
        "marketing",
        true,
    )
    .await;

    try_process(&ctx, &claimed[0])
        .await
        .expect("try_process failed");

    let final_status: Option<String> = sqlx::query_scalar(
        "SELECT final_status FROM comms_request WHERE created_at = $1 AND id = $2",
    )
    .bind(created_at)
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching final_status failed");
    assert_eq!(final_status.as_deref(), Some("sent"));

    let events: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT event_type, provider_ref, provider_status FROM comms_event WHERE comms_request_id = $1",
    )
    .bind(comms_request_id)
    .fetch_all(&tenant.tenant_pool)
    .await
    .expect("fetching comms_event rows failed");
    assert_eq!(
        events,
        vec![(
            "sent".to_string(),
            Some("msg-opted-in".to_string()),
            Some("queued".to_string())
        )]
    );

    tenant.cleanup().await;
}

#[tokio::test]
async fn transactional_sends_without_any_consent_row() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "msg-transactional",
            "status": "queued",
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    let (comms_request_id, created_at, _customer_id) =
        write_outbox_row_with_verification(
            &tenant,
            &vault,
            &cache,
            "+15550100",
            "hello there",
            "transactional",
            Some(Utc::now()),
            unique_name("hmac").as_bytes(),
            None,
            Uuid::new_v4(),
        )
        .await;

    let sender: Arc<dyn Sender> =
        Arc::new(HttpSender::new(mock_server.uri(), "test-key".to_string()));
    let ctx = DispatcherContext {
        pool: tenant.tenant_pool.clone(),
        keystore: Arc::new(vault_keystore()),
        cache: Arc::new(cache),
        mount: tenant.mount.clone(),
        sender,
        verification_mode: "observe".to_string(),
        default_timezone: "UTC".to_string(),
        quiet_hours_policy: None,
        kill_switches: Arc::new(KillSwitchCache::new()),
        draining: Arc::new(RwLock::new(HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(QuotaTracker::new("UTC")),
    };

    let claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].comms_request_id, comms_request_id);

    try_process(&ctx, &claimed[0])
        .await
        .expect("try_process failed");

    let final_status: Option<String> = sqlx::query_scalar(
        "SELECT final_status FROM comms_request WHERE created_at = $1 AND id = $2",
    )
    .bind(created_at)
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching final_status failed");
    assert_eq!(
        final_status.as_deref(),
        Some("sent"),
        "transactional must not require opt-in (§5)"
    );

    let events: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT event_type, provider_ref, provider_status FROM comms_event WHERE comms_request_id = $1",
    )
    .bind(comms_request_id)
    .fetch_all(&tenant.tenant_pool)
    .await
    .expect("fetching comms_event rows failed");
    assert_eq!(
        events,
        vec![(
            "sent".to_string(),
            Some("msg-transactional".to_string()),
            Some("queued".to_string())
        )]
    );

    tenant.cleanup().await;
}

#[tokio::test]
async fn cancelled_row_is_terminal_written_and_not_sent() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "should-not-be-called",
            "status": "queued",
        })))
        .expect(0)
        .mount(&mock_server)
        .await;

    let (comms_request_id, created_at, _customer_id) =
        write_ready_outbox_row(&tenant, &vault, &cache, "+15550100", "hello there")
            .await;

    // Simulates a cancel that landed before the dispatcher claims the row;
    // the same is_cancelled re-check also covers a cancel landing after
    // claim, since it reads fresh every time (T-041, DESIGN.md §6.2).
    sqlx::query("UPDATE outbox SET cancelled_at = now() WHERE comms_request_id = $1")
        .bind(comms_request_id)
        .execute(&tenant.tenant_pool)
        .await
        .expect("setting cancelled_at failed");

    let sender: Arc<dyn Sender> =
        Arc::new(HttpSender::new(mock_server.uri(), "test-key".to_string()));
    let ctx = DispatcherContext {
        pool: tenant.tenant_pool.clone(),
        keystore: Arc::new(vault_keystore()),
        cache: Arc::new(cache),
        mount: tenant.mount.clone(),
        sender,
        verification_mode: "enforce".to_string(),
        default_timezone: "UTC".to_string(),
        quiet_hours_policy: None,
        kill_switches: Arc::new(KillSwitchCache::new()),
        draining: Arc::new(RwLock::new(HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(QuotaTracker::new("UTC")),
    };

    let claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed.len(), 1);

    try_process(&ctx, &claimed[0])
        .await
        .expect("try_process failed");

    let final_status: Option<String> = sqlx::query_scalar(
        "SELECT final_status FROM comms_request WHERE created_at = $1 AND id = $2",
    )
    .bind(created_at)
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching final_status failed");
    assert_eq!(final_status.as_deref(), Some("cancelled"));

    let events: Vec<String> = sqlx::query_scalar(
        "SELECT event_type FROM comms_event WHERE comms_request_id = $1",
    )
    .bind(comms_request_id)
    .fetch_all(&tenant.tenant_pool)
    .await
    .expect("fetching comms_event rows failed");
    assert_eq!(events, vec!["cancelled".to_string()]);

    tenant.cleanup().await;
}

/// Registers a producer directly against the tenant, with no mTLS cert
/// issuance -- unlike `tests/kill_switch.rs`'s heavier `register_test_producer`,
/// nothing in this suite exercises identity resolution.
async fn register_test_producer_row(tenant: &TestTenant, name: &str) -> Uuid {
    let control_url = control_database_url();
    register_producer(
        &tenant.control_pool,
        &control_url,
        &tenant.slug,
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
async fn a_marketing_producer_over_its_per_minute_limit_is_deferred_not_terminal_failed()
 {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = Arc::new(small_cache());
    let control_url = control_database_url();

    let producer_name = unique_name("quota-producer");
    let producer_id = register_test_producer_row(&tenant, &producer_name).await;
    set_producer_quota(
        &tenant.control_pool,
        &control_url,
        &tenant.slug,
        &producer_name,
        "sms",
        "marketing",
        Some(1),
        None,
        enforcement::HARD,
        "test-actor",
    )
    .await
    .expect("setting producer quota failed");

    let quota = QuotaTracker::new("UTC");
    quota
        .refresh_config(&tenant.tenant_pool, Utc::now())
        .await
        .expect("refreshing quota config failed");

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "msg-quota-1",
            "status": "queued",
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    let sender: Arc<dyn Sender> =
        Arc::new(HttpSender::new(mock_server.uri(), "test-key".to_string()));
    let ctx = DispatcherContext {
        pool: tenant.tenant_pool.clone(),
        keystore: Arc::new(vault_keystore()),
        cache: cache.clone(),
        mount: tenant.mount.clone(),
        sender,
        verification_mode: "observe".to_string(),
        default_timezone: "UTC".to_string(),
        quiet_hours_policy: None,
        kill_switches: Arc::new(KillSwitchCache::new()),
        draining: Arc::new(RwLock::new(HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(quota),
    };

    let (first_id, _, _) = write_outbox_row_with_verification(
        &tenant,
        &vault,
        &cache,
        "+15550100",
        "hello there",
        "marketing",
        Some(Utc::now()),
        unique_name("hmac").as_bytes(),
        None,
        producer_id,
    )
    .await;

    let claimed_first = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed_first.len(), 1);
    assert_eq!(claimed_first[0].comms_request_id, first_id);
    insert_consent_row(
        &tenant.tenant_pool,
        claimed_first[0].address_id,
        "marketing",
        true,
    )
    .await;
    try_process(&ctx, &claimed_first[0])
        .await
        .expect("try_process failed for first row");

    let (second_id, second_created_at, _) = write_outbox_row_with_verification(
        &tenant,
        &vault,
        &cache,
        "+15550100",
        "hello there",
        "marketing",
        Some(Utc::now()),
        unique_name("hmac").as_bytes(),
        None,
        producer_id,
    )
    .await;

    let claimed_second = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed_second.len(), 1);
    assert_eq!(claimed_second[0].comms_request_id, second_id);
    insert_consent_row(
        &tenant.tenant_pool,
        claimed_second[0].address_id,
        "marketing",
        true,
    )
    .await;
    try_process(&ctx, &claimed_second[0])
        .await
        .expect("try_process failed for second row");

    let final_status: Option<String> = sqlx::query_scalar(
        "SELECT final_status FROM comms_request WHERE created_at = $1 AND id = $2",
    )
    .bind(second_created_at)
    .bind(second_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching final_status failed");
    assert_eq!(
        final_status, None,
        "a quota-deferred message must not be terminal-written"
    );

    let (leased_until, next_attempt_at): (Option<DateTime<Utc>>, DateTime<Utc>) = sqlx::query_as(
        "SELECT leased_until, next_attempt_at FROM outbox WHERE comms_request_id = $1",
    )
    .bind(second_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching outbox row failed");
    assert_eq!(leased_until, None, "a quota defer must clear the lease");
    assert!(
        next_attempt_at > Utc::now(),
        "a quota-deferred message must be rescheduled into the future"
    );

    tenant.cleanup().await;
}

#[tokio::test]
async fn a_transactional_producer_over_its_per_minute_limit_still_sends_and_is_counted()
{
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = Arc::new(small_cache());
    let control_url = control_database_url();

    let producer_name = unique_name("quota-producer-soft");
    let producer_id = register_test_producer_row(&tenant, &producer_name).await;
    set_producer_quota(
        &tenant.control_pool,
        &control_url,
        &tenant.slug,
        &producer_name,
        "sms",
        "transactional",
        Some(1),
        None,
        enforcement::SOFT,
        "test-actor",
    )
    .await
    .expect("setting producer quota failed");

    let quota = QuotaTracker::new("UTC");
    quota
        .refresh_config(&tenant.tenant_pool, Utc::now())
        .await
        .expect("refreshing quota config failed");

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "msg-quota-2",
            "status": "queued",
        })))
        .expect(2)
        .mount(&mock_server)
        .await;

    let sender: Arc<dyn Sender> =
        Arc::new(HttpSender::new(mock_server.uri(), "test-key".to_string()));
    let ctx = DispatcherContext {
        pool: tenant.tenant_pool.clone(),
        keystore: Arc::new(vault_keystore()),
        cache: cache.clone(),
        mount: tenant.mount.clone(),
        sender,
        verification_mode: "observe".to_string(),
        default_timezone: "UTC".to_string(),
        quiet_hours_policy: None,
        kill_switches: Arc::new(KillSwitchCache::new()),
        draining: Arc::new(RwLock::new(HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(quota),
    };

    for _ in 0..2 {
        let (comms_request_id, created_at, _) = write_outbox_row_with_verification(
            &tenant,
            &vault,
            &cache,
            "+15550100",
            "hello there",
            "transactional",
            Some(Utc::now()),
            unique_name("hmac").as_bytes(),
            None,
            producer_id,
        )
        .await;

        let claimed = repo::claim(
            &tenant.tenant_pool,
            "sms",
            10,
            Utc::now() + chrono::Duration::minutes(2),
            &no_exclusion(),
        )
        .await
        .expect("claim failed");
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].comms_request_id, comms_request_id);
        try_process(&ctx, &claimed[0])
            .await
            .expect("try_process failed");

        let final_status: Option<String> = sqlx::query_scalar(
            "SELECT final_status FROM comms_request WHERE created_at = $1 AND id = $2",
        )
        .bind(created_at)
        .bind(comms_request_id)
        .fetch_one(&tenant.tenant_pool)
        .await
        .expect("fetching final_status failed");
        assert_eq!(
            final_status.as_deref(),
            Some("sent"),
            "AGENTS.md invariant 5: quota must never block transactional traffic, \
             even over a soft limit"
        );
    }

    tenant.cleanup().await;
}

#[tokio::test]
async fn a_producer_with_no_configured_quota_row_is_never_blocked() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = Arc::new(small_cache());

    let producer_id =
        register_test_producer_row(&tenant, &unique_name("quota-unconfigured")).await;

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "msg-quota-3",
            "status": "queued",
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    let sender: Arc<dyn Sender> =
        Arc::new(HttpSender::new(mock_server.uri(), "test-key".to_string()));
    let ctx = DispatcherContext {
        pool: tenant.tenant_pool.clone(),
        keystore: Arc::new(vault_keystore()),
        cache: cache.clone(),
        mount: tenant.mount.clone(),
        sender,
        verification_mode: "observe".to_string(),
        default_timezone: "UTC".to_string(),
        quiet_hours_policy: None,
        kill_switches: Arc::new(KillSwitchCache::new()),
        draining: Arc::new(RwLock::new(HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(QuotaTracker::new("UTC")),
    };

    let (comms_request_id, created_at, _) = write_outbox_row_with_verification(
        &tenant,
        &vault,
        &cache,
        "+15550100",
        "hello there",
        "transactional",
        Some(Utc::now()),
        unique_name("hmac").as_bytes(),
        None,
        producer_id,
    )
    .await;

    let claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed.len(), 1);
    try_process(&ctx, &claimed[0])
        .await
        .expect("try_process failed");

    let final_status: Option<String> = sqlx::query_scalar(
        "SELECT final_status FROM comms_request WHERE created_at = $1 AND id = $2",
    )
    .bind(created_at)
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching final_status failed");
    assert_eq!(final_status.as_deref(), Some("sent"));

    tenant.cleanup().await;
}

#[tokio::test]
async fn a_send_during_quiet_hours_is_deferred_to_window_end_plus_jitter() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "msg-should-not-send",
            "status": "queued",
        })))
        .expect(0)
        .mount(&mock_server)
        .await;

    let (comms_request_id, created_at, _customer_id) =
        write_outbox_row_with_verification(
            &tenant,
            &vault,
            &cache,
            "+15550100",
            "hello there",
            "transactional",
            Some(Utc::now()),
            unique_name("hmac").as_bytes(),
            None,
            Uuid::new_v4(),
        )
        .await;

    // customer.timezone is "UTC" (insert_customer_address's own convention),
    // matching default_timezone below, so the window is evaluated in UTC.
    // Built from "now" so the window always contains it, regardless of when
    // this test runs.
    let now = Utc::now();
    let window_end = now + chrono::Duration::hours(1);
    let policy = QuietHoursPolicy {
        scope: "default".to_string(),
        scope_key: String::new(),
        start_local: (now - chrono::Duration::hours(1)).time(),
        end_local: window_end.time(),
    };

    let sender: Arc<dyn Sender> =
        Arc::new(HttpSender::new(mock_server.uri(), "test-key".to_string()));
    let ctx = DispatcherContext {
        pool: tenant.tenant_pool.clone(),
        keystore: Arc::new(vault_keystore()),
        cache: Arc::new(cache),
        mount: tenant.mount.clone(),
        sender,
        verification_mode: "observe".to_string(),
        default_timezone: "UTC".to_string(),
        quiet_hours_policy: Some(policy),
        kill_switches: Arc::new(KillSwitchCache::new()),
        draining: Arc::new(RwLock::new(HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(QuotaTracker::new("UTC")),
    };

    let claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].comms_request_id, comms_request_id);

    try_process(&ctx, &claimed[0])
        .await
        .expect("try_process failed");

    let row: (Option<DateTime<Utc>>, DateTime<Utc>) = sqlx::query_as(
        "SELECT leased_until, next_attempt_at FROM outbox WHERE comms_request_id = $1",
    )
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("outbox row must still exist, not be deleted");
    let (leased_until, next_attempt_at) = row;
    assert_eq!(leased_until, None, "the lease must be cleared on defer");
    assert!(
        next_attempt_at >= window_end
            && next_attempt_at <= window_end + chrono::Duration::minutes(30),
        "next_attempt_at must land between window end and window end + 30 minutes of jitter"
    );

    let event_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM comms_event WHERE comms_request_id = $1",
    )
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("counting comms_event rows failed");
    assert_eq!(event_count, 0, "a deferred send writes no comms_event row");

    let final_status: Option<String> = sqlx::query_scalar(
        "SELECT final_status FROM comms_request WHERE created_at = $1 AND id = $2",
    )
    .bind(created_at)
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching final_status failed");
    assert_eq!(final_status, None, "a deferred send is not terminal");

    tenant.cleanup().await;
}

#[tokio::test]
async fn a_send_outside_quiet_hours_is_unaffected() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "msg-outside-quiet-hours",
            "status": "queued",
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    let (comms_request_id, created_at, _customer_id) =
        write_outbox_row_with_verification(
            &tenant,
            &vault,
            &cache,
            "+15550100",
            "hello there",
            "transactional",
            Some(Utc::now()),
            unique_name("hmac").as_bytes(),
            None,
            Uuid::new_v4(),
        )
        .await;

    // A one-minute window twelve hours away from "now" never contains it,
    // proving the gate is not a universal block just because a policy is
    // configured.
    let now = Utc::now();
    let policy = QuietHoursPolicy {
        scope: "default".to_string(),
        scope_key: String::new(),
        start_local: (now + chrono::Duration::hours(12)).time(),
        end_local: (now + chrono::Duration::hours(12) + chrono::Duration::minutes(1))
            .time(),
    };

    let sender: Arc<dyn Sender> =
        Arc::new(HttpSender::new(mock_server.uri(), "test-key".to_string()));
    let ctx = DispatcherContext {
        pool: tenant.tenant_pool.clone(),
        keystore: Arc::new(vault_keystore()),
        cache: Arc::new(cache),
        mount: tenant.mount.clone(),
        sender,
        verification_mode: "observe".to_string(),
        default_timezone: "UTC".to_string(),
        quiet_hours_policy: Some(policy),
        kill_switches: Arc::new(KillSwitchCache::new()),
        draining: Arc::new(RwLock::new(HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(QuotaTracker::new("UTC")),
    };

    let claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &no_exclusion(),
    )
    .await
    .expect("claim failed");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].comms_request_id, comms_request_id);

    try_process(&ctx, &claimed[0])
        .await
        .expect("try_process failed");

    let final_status: Option<String> = sqlx::query_scalar(
        "SELECT final_status FROM comms_request WHERE created_at = $1 AND id = $2",
    )
    .bind(created_at)
    .bind(comms_request_id)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching final_status failed");
    assert_eq!(final_status.as_deref(), Some("sent"));

    let outbox_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox WHERE comms_request_id = $1")
            .bind(comms_request_id)
            .fetch_one(&tenant.tenant_pool)
            .await
            .expect("counting outbox rows failed");
    assert_eq!(outbox_count, 0, "a normal send removes the outbox row");

    tenant.cleanup().await;
}

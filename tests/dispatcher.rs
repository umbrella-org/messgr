//! Dispatcher integration suite (DESIGN.md §4.1, §4.2, §4.4, §9, T-013),
//! following `tests/ledger_outbox_schema.rs`/`tests/provider_config.rs`'s
//! conventions: real provisioning against the local stack, no mocks except
//! the provider itself (`wiremock`, the same tool T-012's `HttpSender`
//! tests use).

use std::num::NonZeroUsize;
use std::sync::Arc;
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
use messgr::profile::Profile;
use messgr::sender::Sender;
use messgr::sender::http::HttpSender;
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

    provision_tenant(
        &control_pool,
        &control_url,
        &slug,
        "eu",
        &database_name,
        Profile::Dev,
        "test-actor",
        vault.client(),
    )
    .await
    .expect("provisioning test tenant failed");

    let tenant_pool =
        connect_tenant_pool(&control_url, &database_name, 5, Profile::Dev)
            .await
            .expect("connecting tenant pool failed");

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

/// Writes a real, ready-to-claim `outbox` row the same way `messgr-ingest`
/// would: a real DEK, real AES-256-GCM ciphertexts, and a single-transaction
/// `comms_request` + `outbox` insert (`ingest::repo::insert_transactional`)
/// — rather than standing up mTLS/axum, since this suite is about the
/// dispatcher's read/decrypt/send/write path, not ingest's.
async fn write_ready_outbox_row(
    tenant: &TestTenant,
    vault: &VaultKeyStore,
    cache: &KeyCache,
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

    let outcome = insert_transactional(
        &tenant.tenant_pool,
        &unique_name("idempotency-key"),
        comms_request_id,
        tenant_id,
        customer_id,
        "sms",
        "transactional",
        1,
        "balance-alert",
        1,
        None,
        b"unused-hmac",
        &destination_ciphertext,
        &payload_ciphertext,
        Uuid::new_v4(),
        Uuid::new_v4(),
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

fn small_cache() -> KeyCache {
    KeyCache::new(NonZeroUsize::new(8).unwrap(), Duration::from_secs(60))
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
    };

    let claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
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
async fn failed_send_writes_failed_event_and_final_status_with_no_requeue() {
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
    };

    let claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
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
        vec![("failed".to_string(), None, Some("500".to_string()))]
    );

    let outbox_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox WHERE comms_request_id = $1")
            .bind(comms_request_id)
            .fetch_one(&tenant.tenant_pool)
            .await
            .expect("counting outbox rows failed");
    assert_eq!(
        outbox_count, 0,
        "a single-attempt failure must still be removed from the queue, not requeued"
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
    let (first, second) = tokio::join!(
        repo::claim(&tenant.tenant_pool, "sms", 1, leased_until),
        repo::claim(&tenant.tenant_pool, "sms", 1, leased_until),
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

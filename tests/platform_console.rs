//! Platform console integration suite (T-057), following
//! `tests/admin_panel.rs`'s conventions: real provisioning against the
//! local stack, driven in-process via `tower::ServiceExt::oneshot`.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use messgr::customer_dek::lifecycle::get_or_create_dek;
use messgr::db;
use messgr::encryption;
use messgr::ingest::repo::{InsertOutcome, insert_transactional};
use messgr::key_cache::KeyCache;
use messgr::keystore::VaultKeyStore;
use messgr::platform_auth::mock::MockPlatformProvider;
use messgr::platform_auth::provider::PlatformAuthProvider;
use messgr::platform_console::{self, AppState};
use messgr::producer::register::register_producer;
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
    let _ = sqlx::query(&terminate).execute(control_pool).await;
    let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS \"{database_name}\""))
        .execute(control_pool)
        .await;
    let _ = sqlx::query(
        "DELETE FROM tenant_schema_version WHERE tenant_id = (SELECT id FROM tenant WHERE slug = $1)",
    )
    .bind(slug)
    .execute(control_pool)
    .await;
    let _ = sqlx::query(
        "DELETE FROM platform_audit WHERE tenant_id = (SELECT id FROM tenant WHERE slug = $1)",
    )
    .bind(slug)
    .execute(control_pool)
    .await;
    let _ = sqlx::query(
        "DELETE FROM producer_cert WHERE tenant_id = (SELECT id FROM tenant WHERE slug = $1)",
    )
    .bind(slug)
    .execute(control_pool)
    .await;
    let _ = sqlx::query("DELETE FROM tenant WHERE slug = $1")
        .bind(slug)
        .execute(control_pool)
        .await;
}

struct TestTenant {
    control_pool: PgPool,
    control_url: String,
    tenant_pool: PgPool,
    tenant_id: Uuid,
    slug: String,
    database_name: String,
    mount: String,
}

async fn provision_test_tenant(vault: &VaultKeyStore) -> TestTenant {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");

    let slug = unique_name("test_platform_console");
    let database_name = unique_name("test_db_platform_console");

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
        control_url,
        tenant_pool,
        tenant_id: provision_outcome.tenant_id,
        mount: format!("transit/{slug}"),
        slug,
        database_name,
    }
}

impl TestTenant {
    async fn cleanup(self) {
        self.tenant_pool.close().await;
        drop_test_tenant(&self.control_pool, &self.database_name, &self.slug).await;
    }
}

fn build_router(tenant: &TestTenant, actor: &str, role: &str) -> Router {
    let auth: Arc<dyn PlatformAuthProvider> = Arc::new(MockPlatformProvider::new(
        Profile::Dev,
        actor.to_string(),
        role.to_string(),
    ));
    let state = AppState {
        control_pool: tenant.control_pool.clone(),
        base_db_url: tenant.control_url.clone(),
        auth,
    };
    platform_console::router(state)
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("reading response body failed");
    String::from_utf8(bytes.to_vec()).expect("response body was not valid utf8")
}

async fn seed_comms_request(
    tenant: &TestTenant,
    vault: &VaultKeyStore,
    cache: &KeyCache,
    producer_id: Uuid,
) -> Uuid {
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
    let destination_ciphertext = encryption::encrypt(&dek, aad, b"+15550000000")
        .expect("encrypting destination failed");
    let payload_ciphertext =
        encryption::encrypt(&dek, aad, b"hello").expect("encrypting payload failed");

    let outcome = insert_transactional(
        &tenant.tenant_pool,
        &unique_name("idempotency-key"),
        comms_request_id,
        tenant.tenant_id,
        customer_id,
        "sms",
        messgr::ingest::model::class::TRANSACTIONAL,
        1,
        "balance-alert",
        1,
        None,
        b"destination-hmac-not-checked-by-these-tests",
        &destination_ciphertext,
        &payload_ciphertext,
        producer_id,
        Uuid::new_v4(),
        None,
        None,
    )
    .await
    .expect("insert_transactional failed");

    match outcome {
        InsertOutcome::Created => comms_request_id,
        InsertOutcome::Replayed { .. } => {
            panic!("a fresh idempotency key must never replay")
        }
    }
}

#[tokio::test]
async fn suspend_writes_exactly_one_platform_audit_row() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let router = build_router(&tenant, "operator@example.com", "operator");

    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/tenants/{}/suspend", tenant.tenant_id))
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform_audit WHERE tenant_id = $1 AND action = 'tenant.suspend'",
    )
    .bind(tenant.tenant_id)
    .fetch_one(&tenant.control_pool)
    .await
    .expect("counting platform_audit rows failed");
    assert_eq!(
        count, 1,
        "suspend must write exactly one platform_audit row"
    );

    let status: String = sqlx::query_scalar("SELECT status FROM tenant WHERE id = $1")
        .bind(tenant.tenant_id)
        .fetch_one(&tenant.control_pool)
        .await
        .expect("reading tenant status failed");
    assert_eq!(status, "suspended");

    tenant.cleanup().await;
}

#[tokio::test]
async fn health_view_matches_stats_and_reflects_a_status_mutation() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = KeyCache::new(
        std::num::NonZeroUsize::new(10).unwrap(),
        std::time::Duration::from_secs(60),
    );
    let producer_id = register_producer(
        &tenant.control_pool,
        &tenant.control_url,
        &tenant.slug,
        &unique_name("producer"),
        &format!("CN={}", unique_name("test-producer")),
        "test-team",
        "oncall@example.com",
        "test-actor",
    )
    .await
    .expect("registering producer failed")
    .producer_id;

    let comms_request_id =
        seed_comms_request(&tenant, &vault, &cache, producer_id).await;

    let router = build_router(&tenant, "operator@example.com", "operator");
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/ui/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let before = body_text(response).await;
    assert!(
        before.contains("pending") && before.contains('1'),
        "expected one pending-status row before the mutation, got: {before}"
    );

    sqlx::query("UPDATE comms_request SET final_status = 'delivered' WHERE id = $1")
        .bind(comms_request_id)
        .execute(&tenant.tenant_pool)
        .await
        .expect("mutating final_status failed");

    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/ui/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let after = body_text(response).await;
    assert!(
        after.contains("delivered"),
        "expected the mutated status to show up in the re-rendered view, got: {after}"
    );

    let expected = messgr::stats::tenant_message_stats(
        &tenant.control_pool,
        &tenant.control_url,
        &tenant.slug,
        None,
    )
    .await
    .expect("tenant_message_stats failed");
    assert_eq!(expected.len(), 1);
    assert_eq!(expected[0].status, "delivered");
    assert_eq!(expected[0].count, 1);

    tenant.cleanup().await;
}

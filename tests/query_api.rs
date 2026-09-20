//! `messgr-query-api` integration suite (DESIGN.md §11, T-048), following
//! `tests/dispatcher.rs`'s/`tests/suppression.rs`'s conventions: real
//! provisioning against the local stack, no mocks. Drives the axum `Router`
//! in-process via `tower::ServiceExt::oneshot` — no listening socket needed.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, DurationRound, Utc};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use messgr::auth::mock::MockProvider;
use messgr::auth::provider::{AuthError, AuthProvider, Identity};
use messgr::customer_dek::lifecycle::get_or_create_dek;
use messgr::db;
use messgr::encryption;
use messgr::ingest::repo::{InsertOutcome, insert_transactional};
use messgr::key_cache::KeyCache;
use messgr::keystore::VaultKeyStore;
use messgr::producer::register::register_producer;
use messgr::producer_quota::configure::set_producer_quota;
use messgr::profile::Profile;
use messgr::query_api::{self, AppState};
use messgr::tenant::pool::connect_tenant_pool;
use messgr::tenant::provision::provision_tenant;
use messgr::webhook::TenantPoolCache;

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

    let slug = unique_name("test_query_api");
    let database_name = unique_name("test_db_query_api");

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

fn build_router(
    tenant: &TestTenant,
    keystore: Arc<VaultKeyStore>,
    actor: &str,
    role: &str,
) -> Router {
    let auth: Arc<dyn AuthProvider> = Arc::new(MockProvider::new(
        Profile::Dev,
        actor.to_string(),
        role.to_string(),
    ));
    build_router_with_auth(tenant, keystore, auth)
}

fn build_router_with_auth(
    tenant: &TestTenant,
    keystore: Arc<VaultKeyStore>,
    auth: Arc<dyn AuthProvider>,
) -> Router {
    let state = AppState {
        control_pool: tenant.control_pool.clone(),
        query_api_database_url: tenant.control_url.clone(),
        auth,
        pool_cache: Arc::new(TenantPoolCache::new()),
        tenant_pool_max_connections: 5,
        keystore,
    };
    query_api::router(state)
}

#[allow(clippy::too_many_arguments)]
async fn seed_comms_request(
    tenant: &TestTenant,
    vault: &VaultKeyStore,
    cache: &KeyCache,
    customer_id: Uuid,
    destination: &str,
    body: &str,
    class: &str,
    campaign_id: Option<&str>,
    producer_id: Uuid,
) -> (Uuid, DateTime<Utc>) {
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
        tenant.tenant_id,
        customer_id,
        "sms",
        class,
        1,
        "balance-alert",
        1,
        campaign_id,
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

    let created_at: DateTime<Utc> = match outcome {
        InsertOutcome::Created => {
            sqlx::query_scalar("SELECT created_at FROM comms_request WHERE id = $1")
                .bind(comms_request_id)
                .fetch_one(&tenant.tenant_pool)
                .await
                .expect("fetching created_at failed")
        }
        InsertOutcome::Replayed { .. } => {
            panic!("a fresh idempotency key must never replay")
        }
    };

    (comms_request_id, created_at)
}

async fn register_test_producer(tenant: &TestTenant, name: &str) -> Uuid {
    register_producer(
        &tenant.control_pool,
        &tenant.control_url,
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

/// `form_urlencoded` (what axum's `Query` extractor uses) treats a literal
/// `+` in a query value as an encoded space — `DateTime::to_rfc3339`'s `+`
/// UTC offset sign must be percent-encoded or the timestamp arrives
/// corrupted.
fn url_encode_query_value(value: &str) -> String {
    value.replace('+', "%2B")
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("reading response body failed");
    serde_json::from_slice(&bytes).expect("response body was not valid JSON")
}

#[tokio::test]
async fn customer_service_role_can_only_reach_its_own_customer_timeline() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let vault = Arc::new(vault);

    let own_id = Uuid::new_v4();
    let other_id = Uuid::new_v4();
    let router = build_router(
        &tenant,
        vault.clone(),
        &own_id.to_string(),
        "customer_service",
    );

    let response = router
        .clone()
        .oneshot(
            Request::get(format!("/t/{}/customers/{own_id}/timeline", tenant.slug))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = router
        .clone()
        .oneshot(
            Request::get(format!("/t/{}/customers/{other_id}/timeline", tenant.slug))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let response = router
        .oneshot(
            Request::get(format!("/t/{}/comms", tenant.slug))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    tenant.cleanup().await;
}

#[tokio::test]
async fn compliance_role_search_is_audited() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let vault = Arc::new(vault);

    let router = build_router(&tenant, vault.clone(), "compliance-agent", "compliance");

    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM access_audit")
        .fetch_one(&tenant.tenant_pool)
        .await
        .expect("counting access_audit rows failed");

    let response = router
        .oneshot(
            Request::get(format!("/t/{}/comms?channel=sms", tenant.slug))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM access_audit")
        .fetch_one(&tenant.tenant_pool)
        .await
        .expect("counting access_audit rows failed");
    assert_eq!(after, before + 1);

    let row: (String, String, Option<Uuid>) = sqlx::query_as(
        "SELECT role, route, customer_id FROM access_audit ORDER BY occurred_at DESC LIMIT 1",
    )
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching the new access_audit row failed");
    assert_eq!(row.0, "compliance");
    assert!(row.1.ends_with("/comms"));
    assert_eq!(row.2, None);

    tenant.cleanup().await;
}

#[tokio::test]
async fn comms_detail_requires_matching_created_at() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = KeyCache::new(
        std::num::NonZeroUsize::new(10).unwrap(),
        std::time::Duration::from_secs(60),
    );

    let customer_id = Uuid::new_v4();
    let producer_id = register_test_producer(&tenant, &unique_name("producer")).await;
    let (comms_request_id, created_at) = seed_comms_request(
        &tenant,
        &vault,
        &cache,
        customer_id,
        "+15551234567",
        "your balance is $42",
        "transactional",
        None,
        producer_id,
    )
    .await;

    let vault = Arc::new(vault);
    let router = build_router(&tenant, vault.clone(), "compliance-agent", "compliance");

    let response = router
        .clone()
        .oneshot(
            Request::get(format!(
                "/t/{}/comms/{comms_request_id}?created_at={}",
                tenant.slug,
                url_encode_query_value(&created_at.to_rfc3339())
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["id"], comms_request_id.to_string());
    assert_eq!(json["body"], "your balance is $42");

    let wrong_created_at = created_at + chrono::Duration::seconds(1);
    let response = router
        .oneshot(
            Request::get(format!(
                "/t/{}/comms/{comms_request_id}?created_at={}",
                tenant.slug,
                url_encode_query_value(&wrong_created_at.to_rfc3339())
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    tenant.cleanup().await;
}

/// Regression for T-048 rework finding F4: the timeline UI's own detail
/// link must be one the API actually accepts. Renders the real `/ui/...`
/// page, pulls the generated `?created_at=` value straight out of the HTML
/// (no re-deriving it from the seeded timestamp), and follows it.
#[tokio::test]
async fn ui_timeline_detail_link_is_followable() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = KeyCache::new(
        std::num::NonZeroUsize::new(10).unwrap(),
        std::time::Duration::from_secs(60),
    );

    let customer_id = Uuid::new_v4();
    let producer_id = register_test_producer(&tenant, &unique_name("producer")).await;
    let (comms_request_id, _) = seed_comms_request(
        &tenant,
        &vault,
        &cache,
        customer_id,
        "+15551234567",
        "your balance is $42",
        "transactional",
        None,
        producer_id,
    )
    .await;

    let vault = Arc::new(vault);
    let router = build_router(&tenant, vault.clone(), "compliance-agent", "compliance");

    let timeline_response = router
        .clone()
        .oneshot(
            Request::get(format!(
                "/t/{}/ui/customers/{customer_id}/timeline",
                tenant.slug
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(timeline_response.status(), StatusCode::OK);
    let html_bytes = axum::body::to_bytes(timeline_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let html =
        String::from_utf8(html_bytes.to_vec()).expect("timeline HTML was not UTF-8");

    let href_marker = format!("comms/{comms_request_id}?created_at=");
    let query_start = html
        .find(&href_marker)
        .expect("timeline HTML has no detail link for the seeded row")
        + href_marker.len();
    let query_value = &html[query_start..];
    let query_value =
        &query_value[..query_value.find('"').expect("href has no closing quote")];

    let detail_response = router
        .oneshot(
            Request::get(format!(
                "/t/{}/comms/{comms_request_id}?created_at={query_value}",
                tenant.slug
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        detail_response.status(),
        StatusCode::OK,
        "the timeline UI's own generated link must be accepted by GET /comms/{{id}}"
    );
    let json = body_json(detail_response).await;
    assert_eq!(json["id"], comms_request_id.to_string());

    tenant.cleanup().await;
}

#[tokio::test]
async fn customer_timeline_includes_rows_written_under_a_merged_alias() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = KeyCache::new(
        std::num::NonZeroUsize::new(10).unwrap(),
        std::time::Duration::from_secs(60),
    );

    let canonical_id = Uuid::new_v4();
    let old_id = Uuid::new_v4();

    sqlx::query(
        "INSERT INTO customer (id, locale, timezone, provisional, created_at) VALUES ($1, 'en-US', 'UTC', false, now())",
    )
    .bind(canonical_id)
    .execute(&tenant.tenant_pool)
    .await
    .expect("inserting canonical customer failed");

    sqlx::query(
        "INSERT INTO customer_alias (old_customer_id, customer_id, merged_at) VALUES ($1, $2, now())",
    )
    .bind(old_id)
    .bind(canonical_id)
    .execute(&tenant.tenant_pool)
    .await
    .expect("inserting customer_alias failed");

    let producer_id = register_test_producer(&tenant, &unique_name("producer")).await;
    seed_comms_request(
        &tenant,
        &vault,
        &cache,
        old_id,
        "+15550001111",
        "hello under the old id",
        "transactional",
        None,
        producer_id,
    )
    .await;

    let vault = Arc::new(vault);
    let router = build_router(&tenant, vault.clone(), "compliance-agent", "compliance");

    let response = router
        .oneshot(
            Request::get(format!(
                "/t/{}/customers/{canonical_id}/timeline",
                tenant.slug
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    let rows = json
        .as_array()
        .expect("timeline response must be a JSON array");
    assert!(
        rows.iter()
            .any(|row| row["customer_id"] == old_id.to_string()),
        "expected the row written under the merged old id to appear in the canonical timeline, got {json:?}"
    );

    tenant.cleanup().await;
}

#[tokio::test]
async fn campaign_reach_aggregates_by_final_status() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = KeyCache::new(
        std::num::NonZeroUsize::new(10).unwrap(),
        std::time::Duration::from_secs(60),
    );

    let campaign_id = unique_name("campaign");
    let producer_id = register_test_producer(&tenant, &unique_name("producer")).await;

    let (sent_id, _) = seed_comms_request(
        &tenant,
        &vault,
        &cache,
        Uuid::new_v4(),
        "+15550001",
        "offer A",
        "marketing",
        Some(&campaign_id),
        producer_id,
    )
    .await;
    let (failed_id, _) = seed_comms_request(
        &tenant,
        &vault,
        &cache,
        Uuid::new_v4(),
        "+15550002",
        "offer A",
        "marketing",
        Some(&campaign_id),
        producer_id,
    )
    .await;
    // Third row left in-flight (final_status stays NULL).
    seed_comms_request(
        &tenant,
        &vault,
        &cache,
        Uuid::new_v4(),
        "+15550003",
        "offer A",
        "marketing",
        Some(&campaign_id),
        producer_id,
    )
    .await;

    sqlx::query("UPDATE comms_request SET final_status = 'sent', finalized_at = now() WHERE id = $1")
        .bind(sent_id)
        .execute(&tenant.tenant_pool)
        .await
        .expect("marking row sent failed");
    sqlx::query("UPDATE comms_request SET final_status = 'failed', finalized_at = now() WHERE id = $1")
        .bind(failed_id)
        .execute(&tenant.tenant_pool)
        .await
        .expect("marking row failed failed");

    let vault = Arc::new(vault);
    let router = build_router(&tenant, vault.clone(), "campaign-agent", "campaign_ops");

    let response = router
        .oneshot(
            Request::get(format!("/t/{}/campaigns/{campaign_id}/reach", tenant.slug))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    let counts = json
        .as_array()
        .expect("campaign reach response must be a JSON array");
    assert_eq!(counts.len(), 3);
    let find = |status: Option<&str>| {
        counts
            .iter()
            .find(|row| row["final_status"].as_str() == status)
            .map(|row| row["count"].as_i64().unwrap())
    };
    assert_eq!(find(Some("sent")), Some(1));
    assert_eq!(find(Some("failed")), Some(1));
    assert_eq!(find(None), Some(1));

    let customer_service_router = build_router(
        &tenant,
        vault,
        &Uuid::new_v4().to_string(),
        "customer_service",
    );
    let response = customer_service_router
        .oneshot(
            Request::get(format!("/t/{}/campaigns/{campaign_id}/reach", tenant.slug))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    tenant.cleanup().await;
}

#[tokio::test]
async fn producers_usage_and_quota_are_comms_ops_only() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;

    let producer_name = unique_name("producer");
    let producer_id = register_test_producer(&tenant, &producer_name).await;
    set_producer_quota(
        &tenant.control_pool,
        &tenant.control_url,
        &tenant.slug,
        &producer_name,
        "sms",
        "marketing",
        Some(100),
        Some(1000),
        "hard",
        "test-actor",
    )
    .await
    .expect("setting producer quota failed");

    let now = Utc::now();
    let minute_start = now
        .duration_trunc(chrono::TimeDelta::minutes(1))
        .expect("minute truncation is infallible");
    let day_start = now
        .duration_trunc(chrono::TimeDelta::days(1))
        .expect("day truncation is infallible");
    sqlx::query(
        "INSERT INTO producer_usage (producer_id, channel, class, granularity, window_start, sent, blocked) \
         VALUES ($1, 'sms', 'marketing', 'minute', $2, 7, 1)",
    )
    .bind(producer_id)
    .bind(minute_start)
    .execute(&tenant.tenant_pool)
    .await
    .expect("seeding producer_usage minute row failed");
    sqlx::query(
        "INSERT INTO producer_usage (producer_id, channel, class, granularity, window_start, sent, blocked) \
         VALUES ($1, 'sms', 'marketing', 'day', $2, 42, 3)",
    )
    .bind(producer_id)
    .bind(day_start)
    .execute(&tenant.tenant_pool)
    .await
    .expect("seeding producer_usage day row failed");

    let vault = Arc::new(vault);

    for role in ["compliance", "campaign_ops"] {
        let router = build_router(&tenant, vault.clone(), "agent", role);
        let response = router
            .clone()
            .oneshot(
                Request::get(format!(
                    "/t/{}/producers/{producer_id}/usage",
                    tenant.slug
                ))
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "role {role} must not read producer usage"
        );

        let response = router
            .oneshot(
                Request::get(format!(
                    "/t/{}/producers/{producer_id}/quota?channel=sms&class=marketing",
                    tenant.slug
                ))
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "role {role} must not read producer quota"
        );
    }

    let comms_ops_router =
        build_router(&tenant, vault.clone(), "ops-agent", "comms_ops");

    let response = comms_ops_router
        .clone()
        .oneshot(
            Request::get(format!("/t/{}/producers/{producer_id}/usage", tenant.slug))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    let rows = json
        .as_array()
        .expect("usage response must be a JSON array");
    assert!(
        rows.iter()
            .any(|row| row["granularity"] == "minute" && row["sent"] == 7)
    );
    assert!(
        rows.iter()
            .any(|row| row["granularity"] == "day" && row["sent"] == 42)
    );

    let response = comms_ops_router
        .oneshot(
            Request::get(format!(
                "/t/{}/producers/{producer_id}/quota?channel=sms&class=marketing",
                tenant.slug
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["per_minute"], 100);
    assert_eq!(json["per_day"], 1000);

    tenant.cleanup().await;
}

/// Always refuses — proves `GET /openapi.yaml` never calls into the
/// configured `AuthProvider` at all (decision 9), rather than merely
/// happening to succeed against `MockProvider`.
struct AlwaysRejectProvider;

#[async_trait::async_trait]
impl AuthProvider for AlwaysRejectProvider {
    async fn authenticate(&self, _tenant_id: Uuid) -> Result<Identity, AuthError> {
        Err(AuthError::Unauthenticated)
    }
}

#[tokio::test]
async fn openapi_yaml_is_served_without_authentication() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let vault = Arc::new(vault);

    let router = build_router_with_auth(&tenant, vault, Arc::new(AlwaysRejectProvider));

    let response = router
        .oneshot(
            Request::get(format!("/t/{}/openapi.yaml", tenant.slug))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    tenant.cleanup().await;
}

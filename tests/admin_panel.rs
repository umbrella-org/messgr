//! Admin panel integration suite (T-049), following `tests/query_api.rs`'s
//! conventions: real provisioning against the local stack, no mocks, driven
//! in-process via `tower::ServiceExt::oneshot`. Each `tests/*.rs` file is its
//! own binary, so the handful of shared helpers are duplicated rather than
//! imported from `tests/query_api.rs`.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use messgr::auth::mock::MockProvider;
use messgr::auth::provider::AuthProvider;
use messgr::customer_dek::lifecycle::get_or_create_dek;
use messgr::db;
use messgr::encryption;
use messgr::ingest::repo::{InsertOutcome, insert_transactional};
use messgr::key_cache::KeyCache;
use messgr::keystore::VaultKeyStore;
use messgr::producer::register::register_producer;
use messgr::producer_quota::repo as producer_quota_repo;
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
    // T-058: platform_kill_switch references tenant -- delete it first.
    let _ = sqlx::query(
        "DELETE FROM platform_kill_switch WHERE tenant_id = (SELECT id FROM tenant WHERE slug = $1)",
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

    let slug = unique_name("test_admin_panel");
    let database_name = unique_name("test_db_admin_panel");

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

async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("reading response body failed");
    String::from_utf8(bytes.to_vec()).expect("response body was not valid utf8")
}

/// Seeds one `outbox`/`comms_request` row for `producer_id`, with an
/// explicit `next_attempt_at` (via `scheduled_for`) so ordering across
/// multiple seeded rows is deterministic rather than racing on `now()`.
#[allow(clippy::too_many_arguments)]
async fn seed_outbox_row(
    tenant: &TestTenant,
    vault: &VaultKeyStore,
    cache: &KeyCache,
    producer_id: Uuid,
    class: &str,
    campaign_id: Option<&str>,
    scheduled_for: Option<DateTime<Utc>>,
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
        scheduled_for,
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

async fn active_kill_switch_id(
    tenant: &TestTenant,
    scope: &str,
    scope_key: Option<&str>,
) -> Uuid {
    sqlx::query_scalar(
        "SELECT id FROM kill_switch WHERE scope = $1 AND scope_key IS NOT DISTINCT FROM $2 \
         AND released_at IS NULL",
    )
    .bind(scope)
    .bind(scope_key)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("expected an active kill switch row")
}

const NON_COMMS_OPS_ROLES: &[&str] =
    &["admin", "compliance", "customer_service", "campaign_ops"];

#[tokio::test]
async fn kill_switch_engage_is_comms_ops_only_and_blast_radius_matches_scope() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = KeyCache::new(
        std::num::NonZeroUsize::new(10).unwrap(),
        std::time::Duration::from_secs(60),
    );

    let producer_a = register_test_producer(&tenant, &unique_name("producer_a")).await;
    let producer_b = register_test_producer(&tenant, &unique_name("producer_b")).await;

    seed_outbox_row(&tenant, &vault, &cache, producer_a, "marketing", None, None).await;
    seed_outbox_row(&tenant, &vault, &cache, producer_a, "marketing", None, None).await;
    seed_outbox_row(&tenant, &vault, &cache, producer_b, "marketing", None, None).await;

    let vault = Arc::new(vault);
    let engage_body = format!(
        "scope=producer&scope_key={producer_a}&on_queued=hold&reason=incident-drill"
    );

    for role in NON_COMMS_OPS_ROLES {
        let router = build_router(&tenant, vault.clone(), "agent", role);
        let response = router
            .oneshot(
                Request::post(format!("/t/{}/admin/kill-switches", tenant.slug))
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(engage_body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "role {role} must not engage a kill switch"
        );
    }

    let comms_ops_router =
        build_router(&tenant, vault.clone(), "ops-agent", "comms_ops");
    let response = comms_ops_router
        .clone()
        .oneshot(
            Request::post(format!("/t/{}/admin/kill-switches", tenant.slug))
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(engage_body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Blast radius for producer_a's scope must show 2 marketing rows; for a
    // scope matching nothing (producer_b was never engaged), zero.
    let response = comms_ops_router
        .clone()
        .oneshot(
            Request::get(format!(
                "/t/{}/ui/admin/kill-switches/blast-radius?scope=producer&scope_key={producer_a}",
                tenant.slug
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_text(response).await;
    assert!(
        body.contains("marketing"),
        "expected a marketing row: {body}"
    );
    assert!(
        body.contains("<td>2</td>"),
        "expected count 2 for producer_a's scope: {body}"
    );

    let response = comms_ops_router
        .clone()
        .oneshot(
            Request::get(format!(
                "/t/{}/ui/admin/kill-switches/blast-radius?scope=producer&scope_key={producer_b}",
                tenant.slug
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_text(response).await;
    assert!(
        body.contains("<td>1</td>"),
        "expected producer_b's own scope to show only its own 1 row, isolated from producer_a's: {body}"
    );

    // Double-engage the same scope must be rejected, not a raw 500.
    let response = comms_ops_router
        .clone()
        .oneshot(
            Request::post(format!("/t/{}/admin/kill-switches", tenant.slug))
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(engage_body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);

    // Release, then a second release of the same id is idempotent.
    let switch_id =
        active_kill_switch_id(&tenant, "producer", Some(&producer_a.to_string())).await;
    let response = comms_ops_router
        .clone()
        .oneshot(
            Request::post(format!(
                "/t/{}/admin/kill-switches/{switch_id}/release",
                tenant.slug
            ))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = comms_ops_router
        .oneshot(
            Request::post(format!(
                "/t/{}/admin/kill-switches/{switch_id}/release",
                tenant.slug
            ))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "second release must be idempotent"
    );

    tenant.cleanup().await;
}

#[tokio::test]
async fn producer_registry_is_admin_only_and_quota_override_round_trips() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let vault = Arc::new(vault);

    let name = unique_name("producer");
    let register_body = format!(
        "name={name}&cert_subject=CN%3D{name}&owner_team=payments&contact=oncall%40example.com"
    );

    let comms_ops_router =
        build_router(&tenant, vault.clone(), "ops-agent", "comms_ops");
    let response = comms_ops_router
        .oneshot(
            Request::post(format!("/t/{}/admin/producers", tenant.slug))
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(register_body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let admin_router = build_router(&tenant, vault.clone(), "admin-agent", "admin");
    let response = admin_router
        .clone()
        .oneshot(
            Request::post(format!("/t/{}/admin/producers", tenant.slug))
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(register_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let producer_id: Uuid =
        sqlx::query_scalar("SELECT id FROM producer WHERE name = $1")
            .bind(&name)
            .fetch_one(&tenant.tenant_pool)
            .await
            .expect("registered producer must exist");

    // class = "auth" must stay unreachable through the new route too.
    let response = admin_router
        .clone()
        .oneshot(
            Request::post(format!(
                "/t/{}/admin/producers/{producer_id}/quota",
                tenant.slug
            ))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(
                "channel=sms&class=auth&per_minute=10&per_day=100&enforcement=soft",
            ))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let response = admin_router
        .clone()
        .oneshot(
            Request::post(format!(
                "/t/{}/admin/producers/{producer_id}/quota",
                tenant.slug
            ))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(
                "channel=sms&class=marketing&per_minute=10&per_day=100&enforcement=soft",
            ))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let quota = producer_quota_repo::load_one(
        &tenant.tenant_pool,
        producer_id,
        "sms",
        "marketing",
    )
    .await
    .expect("loading quota failed")
    .expect("quota row must exist after set");
    assert_eq!(quota.per_minute, Some(10));
    assert_eq!(quota.per_day, Some(100));

    let response = admin_router
        .oneshot(
            Request::post(format!(
                "/t/{}/admin/producers/{producer_id}/quota/override",
                tenant.slug
            ))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(
                "channel=sms&class=marketing&per_day=500&valid_from=2026-01-01T00%3A00&\
                 valid_to=2026-01-02T00%3A00&approved_by=manager&reason=holiday-spike",
            ))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let overrides = producer_quota_repo::list_overrides(&tenant.tenant_pool)
        .await
        .expect("listing overrides failed");
    assert_eq!(overrides.len(), 1);
    assert_eq!(overrides[0].producer_id, producer_id);
    assert_eq!(overrides[0].per_day, 500);

    tenant.cleanup().await;
}

#[tokio::test]
async fn scheduled_queue_filters_by_producer_and_cancel_removes_row() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = KeyCache::new(
        std::num::NonZeroUsize::new(10).unwrap(),
        std::time::Duration::from_secs(60),
    );

    let producer_a = register_test_producer(&tenant, &unique_name("producer_a")).await;
    let producer_b = register_test_producer(&tenant, &unique_name("producer_b")).await;

    let now = Utc::now();
    let first = seed_outbox_row(
        &tenant,
        &vault,
        &cache,
        producer_a,
        "marketing",
        None,
        Some(now + Duration::hours(1)),
    )
    .await;
    let second = seed_outbox_row(
        &tenant,
        &vault,
        &cache,
        producer_a,
        "marketing",
        None,
        Some(now + Duration::hours(2)),
    )
    .await;
    seed_outbox_row(
        &tenant,
        &vault,
        &cache,
        producer_b,
        "marketing",
        None,
        Some(now + Duration::hours(1)),
    )
    .await;

    let vault = Arc::new(vault);
    let router = build_router(&tenant, vault, "ops-agent", "comms_ops");

    let response = router
        .clone()
        .oneshot(
            Request::get(format!(
                "/t/{}/ui/admin/scheduled?producer_id={producer_a}",
                tenant.slug
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_text(response).await;
    assert!(body.contains(&first.to_string()));
    assert!(body.contains(&second.to_string()));
    assert!(!body.contains(&producer_b.to_string()));

    let response = router
        .clone()
        .oneshot(
            Request::post(format!("/t/{}/admin/scheduled/{first}/cancel", tenant.slug))
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!("producer_id={producer_a}")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let cancelled_at: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT cancelled_at FROM outbox WHERE comms_request_id = $1",
    )
    .bind(first)
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("row must still exist after cancel");
    assert!(cancelled_at.is_some());

    let response = router
        .oneshot(
            Request::get(format!(
                "/t/{}/ui/admin/scheduled?producer_id={producer_a}",
                tenant.slug
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    let body = body_text(response).await;
    assert!(
        !body.contains(&first.to_string()),
        "cancelled row must drop out of the listing"
    );
    assert!(body.contains(&second.to_string()));

    tenant.cleanup().await;
}

/// T-058: a platform switch shows on the tenant's own kill-switch page as a
/// distinct, non-actionable suspension -- a generic label, never the
/// operator's reason, and no release control -- on the plain view and on
/// the page re-rendered after the tenant engages its own switch.
#[tokio::test]
async fn platform_suspension_is_shown_as_a_non_actionable_generic_block() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;

    let (outcome, _) = messgr::platform_kill_switch::configure::engage(
        &tenant.control_pool,
        &tenant.control_url,
        messgr::platform_kill_switch::model::scope::TENANT,
        Some(tenant.tenant_id),
        "non-payment since march",
        "operator@example.com",
    )
    .await
    .expect("engaging platform switch failed");

    let vault = Arc::new(vault);
    let router = build_router(&tenant, vault.clone(), "ops-agent", "comms_ops");
    let response = router
        .oneshot(
            Request::get(format!("/t/{}/ui/admin/kill-switches", tenant.slug))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_text(response).await;
    assert!(body.contains("Suspended by the platform operator"));
    assert!(
        !body.contains("non-payment since march"),
        "the operator's reason is provider-internal and must not reach the tenant"
    );
    assert!(
        !body.contains(&outcome.id.to_string()),
        "the platform switch must offer no release control (its id never appears)"
    );

    let router = build_router(&tenant, vault.clone(), "ops-agent", "comms_ops");
    let response = router
        .oneshot(
            Request::post(format!("/t/{}/admin/kill-switches", tenant.slug))
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("scope=global&on_queued=hold&reason=own-drill"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        body_text(response)
            .await
            .contains("Suspended by the platform operator"),
        "the block must survive the re-render after a tenant engage"
    );

    tenant.cleanup().await;
}

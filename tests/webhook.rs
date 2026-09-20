//! `messgr-webhook` integration suite (DESIGN.md §10, T-047), following
//! `tests/ingest.rs`'s `TestServer` conventions (real provisioning, real
//! dev-mode Vault, no mocks) but without mTLS -- `messgr-webhook`
//! authenticates a caller by its per-tenant HMAC signature, not a client
//! certificate.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::routing::post;
use axum_server::Handle;
use axum_server::tls_rustls::RustlsConfig;
use chrono::Utc;
use hmac::{Hmac, KeyInit, Mac};
use reqwest::Certificate;
use sha2::Sha256;
use sqlx::PgPool;
use uuid::Uuid;

use messgr::customer_dek::lifecycle::get_or_create_dek;
use messgr::db;
use messgr::encryption;
use messgr::key_cache::KeyCache;
use messgr::keystore::VaultKeyStore;
use messgr::mtls;
use messgr::producer::dev_pki;
use messgr::profile::Profile;
use messgr::tenant::provision::provision_tenant;
use messgr::tenant::repo as tenant_repo;
use messgr::webhook::handler::receive_webhook;
use messgr::webhook::{AppState, TenantPoolCache};
use messgr::webhook_receipt::promote::run_for_tenant as run_webhook_promote;
use messgr::webhook_verify::{HmacSha256Verifier, WebhookVerifier};

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

fn unique_hostname(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
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

/// A running `messgr-webhook` router, wired exactly like `bin/webhook.rs`
/// (minus env-var plumbing), on an ephemeral port.
struct TestServer {
    addr: SocketAddr,
    handle: Handle,
}

impl TestServer {
    async fn start(
        control_pool: PgPool,
        control_url: String,
        server_cert_pem: &str,
        server_key_pem: &str,
        cert_dir: &Path,
    ) -> Self {
        let cert_path = cert_dir.join("server-cert.pem");
        let key_path = cert_dir.join("server-key.pem");
        std::fs::write(&cert_path, server_cert_pem)
            .expect("writing server cert failed");
        std::fs::write(&key_path, server_key_pem).expect("writing server key failed");

        let vault_keystore = Arc::new(vault_keystore());
        let verifier: Arc<dyn WebhookVerifier> = Arc::new(HmacSha256Verifier);
        let app_state = AppState {
            control_pool,
            control_database_url: control_url,
            vault_keystore,
            verifier,
            pool_cache: Arc::new(TenantPoolCache::new()),
            tenant_pool_max_connections: 5,
        };
        let app: Router = Router::new()
            .route("/webhook/{webhook_token}/{provider}", post(receive_webhook))
            .with_state(app_state);

        let tls_config = mtls::load_plain_server_config(
            cert_path.to_str().unwrap(),
            key_path.to_str().unwrap(),
        )
        .expect("loading test server TLS config failed");
        let rustls_config = RustlsConfig::from_config(Arc::new(tls_config));

        let handle = Handle::new();
        let bind_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server_handle = handle.clone();
        tokio::spawn(async move {
            axum_server::bind_rustls(bind_addr, rustls_config)
                .handle(server_handle)
                .serve(app.into_make_service())
                .await
        });

        let addr = handle
            .listening()
            .await
            .expect("server never reported a listening address");

        TestServer { addr, handle }
    }

    fn shutdown(&self) {
        self.handle.shutdown();
    }
}

/// A plain `reqwest::Client` (no client identity -- `messgr-webhook` takes
/// no client certificate) trusting `server_ca_pem` and resolving `hostname`
/// to the test server's actual ephemeral address.
fn plain_client(
    hostname: &str,
    addr: SocketAddr,
    server_ca_pem: &str,
) -> reqwest::Client {
    let ca_cert = Certificate::from_pem(server_ca_pem.as_bytes())
        .expect("parsing server CA failed");

    reqwest::Client::builder()
        .add_root_certificate(ca_cert)
        .resolve(hostname, addr)
        .build()
        .expect("building reqwest client failed")
}

fn sign(secret: &[u8], body: &[u8]) -> String {
    let mut mac =
        <Hmac<Sha256>>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body);
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

struct Fixture {
    control_pool: PgPool,
    control_url: String,
    tenant_slug: String,
    database_name: String,
    webhook_token: String,
    webhook_secret: Vec<u8>,
    server_common_name: String,
    server: TestServer,
    server_ca_pem: String,
    cert_dir: std::path::PathBuf,
}

impl Fixture {
    fn client(&self) -> reqwest::Client {
        plain_client(
            &self.server_common_name,
            self.server.addr,
            &self.server_ca_pem,
        )
    }

    fn url(&self, provider: &str) -> String {
        format!(
            "https://{}/webhook/{}/{}",
            self.server_common_name, self.webhook_token, provider
        )
    }

    async fn tenant_pool(&self) -> PgPool {
        let tenant = tenant_repo::find_by_slug(&self.control_pool, &self.tenant_slug)
            .await
            .expect("looking up the tenant failed")
            .expect("the tenant must still exist");
        messgr::tenant::pool::connect_tenant_pool(
            &self.control_pool,
            &self.control_url,
            tenant.id,
            &tenant.database_name,
            5,
        )
        .await
        .expect("connecting the tenant pool failed")
        .pool
    }
}

/// Provisions a tenant, writes its webhook shared secret to Vault KV, boots
/// dev PKI, mints a server certificate, and starts a `TestServer`.
async fn setup() -> Fixture {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("connecting to control database failed");
    let vault = vault_keystore();

    let tenant_slug = unique_name("test_tenant_webhook");
    let database_name = unique_name("test_db_webhook");

    provision_tenant(
        &control_pool,
        &control_url,
        &tenant_slug,
        "eu",
        &database_name,
        "test-actor",
        vault.client(),
    )
    .await
    .expect("provisioning test tenant failed");

    let tenant = tenant_repo::find_by_slug(&control_pool, &tenant_slug)
        .await
        .expect("looking up the provisioned tenant failed")
        .expect("the tenant must exist immediately after provisioning");

    let webhook_secret = b"webhook-shared-secret".to_vec();
    vaultrs::kv2::set(
        vault.client(),
        "secret",
        &format!("{tenant_slug}/webhook"),
        &serde_json::json!({"secret": String::from_utf8(webhook_secret.clone()).unwrap()}),
    )
    .await
    .expect("writing the tenant's webhook secret to Vault failed");

    dev_pki::bootstrap(vault.client(), Profile::Dev)
        .await
        .expect("bootstrapping dev PKI failed");

    let server_common_name = unique_hostname("messgr-webhook.test");
    let server_cert =
        dev_pki::issue_server_cert(vault.client(), Profile::Dev, &server_common_name)
            .await
            .expect("issuing server certificate failed");

    let cert_dir = std::env::temp_dir().join(unique_name("messgr-webhook-test"));
    std::fs::create_dir_all(&cert_dir).expect("creating temp cert dir failed");

    let server = TestServer::start(
        control_pool.clone(),
        control_url.clone(),
        &server_cert.certificate,
        &server_cert.private_key,
        &cert_dir,
    )
    .await;

    Fixture {
        control_pool,
        control_url,
        tenant_slug,
        database_name,
        webhook_token: tenant.webhook_token,
        webhook_secret,
        server_common_name,
        server,
        server_ca_pem: server_cert.issuing_ca,
        cert_dir,
    }
}

async fn teardown(fixture: Fixture) {
    fixture.server.shutdown();
    let _ = std::fs::remove_dir_all(&fixture.cert_dir);
    drop_test_tenant(
        &fixture.control_pool,
        &fixture.database_name,
        &fixture.tenant_slug,
    )
    .await;
}

async fn insert_comms_request(
    pool: &PgPool,
    id: Uuid,
    customer_id: Uuid,
    created_at: chrono::DateTime<Utc>,
    final_status: Option<&str>,
) {
    sqlx::query(
        r#"
        INSERT INTO comms_request (
            tenant_id, id, created_at, customer_id, channel, class, template_id,
            template_version, destination_hmac, destination_ciphertext, producer_id, final_status
        ) VALUES (
            $1, $2, $3, $4, 'sms', 'transactional', 'welcome', 1, $5, $6, $7, $8
        )
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(id)
    .bind(created_at)
    .bind(customer_id)
    .bind(b"hmac".to_vec())
    .bind(b"ciphertext".to_vec())
    .bind(Uuid::new_v4())
    .bind(final_status)
    .execute(pool)
    .await
    .expect("inserting comms_request failed");
}

async fn insert_comms_event(
    pool: &PgPool,
    comms_request_id: Uuid,
    customer_id: Uuid,
    occurred_at: chrono::DateTime<Utc>,
    provider_ref: &str,
) {
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
    .execute(pool)
    .await
    .expect("inserting comms_event failed");
}

async fn staging_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM webhook_receipt_staging")
        .fetch_one(pool)
        .await
        .expect("counting webhook_receipt_staging rows failed")
}

fn receipt_body(
    provider_ref: &str,
    event_type: &str,
    occurred_at: chrono::DateTime<Utc>,
) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "provider_ref": provider_ref,
        "event_type": event_type,
        "occurred_at": occurred_at.to_rfc3339(),
        "provider_status": "DELIVERED",
    }))
    .expect("serializing the mock receipt body failed")
}

// Scenario 1: a correctly-signed mock receipt -> a webhook_receipt_staging
// row appears; running the webhook-promote step then removes it and leaves
// a matching comms_event row whose ciphertext decrypts back to the original
// payload under the customer's DEK.
#[tokio::test]
async fn signed_receipt_stages_then_promotes_into_comms_event() {
    let fixture = setup().await;
    let pool = fixture.tenant_pool().await;
    let vault = vault_keystore();

    let comms_request_id = Uuid::new_v4();
    let customer_id = Uuid::new_v4();
    let created_at = Utc::now();
    let occurred_at = Utc::now();
    insert_comms_request(
        &pool,
        comms_request_id,
        customer_id,
        created_at,
        Some("sent"),
    )
    .await;
    insert_comms_event(&pool, comms_request_id, customer_id, occurred_at, "abc").await;

    let body = receipt_body("abc", "delivered", occurred_at);
    let signature = sign(&fixture.webhook_secret, &body);

    let response = fixture
        .client()
        .post(fixture.url("mock-provider"))
        .header("x-webhook-signature", signature)
        .body(body.clone())
        .send()
        .await
        .expect("sending the signed webhook request failed");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(staging_count(&pool).await, 1);

    let report = run_webhook_promote(
        &fixture.control_pool,
        &fixture.control_url,
        &fixture.tenant_slug,
        &vault,
        5,
    )
    .await
    .expect("webhook promote run failed");
    assert_eq!(report.promoted, 1);
    assert_eq!(report.orphaned, 0);
    assert_eq!(staging_count(&pool).await, 0);

    let (ciphertext, final_status): (Option<Vec<u8>>, Option<String>) = sqlx::query_as(
        r#"
        SELECT ce.provider_payload_ciphertext, cr.final_status
        FROM comms_event ce
        JOIN comms_request cr ON cr.id = ce.comms_request_id
        WHERE ce.event_type = 'delivered'
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("fetching the promoted comms_event failed");

    let ciphertext =
        ciphertext.expect("promoted comms_event must carry a non-NULL ciphertext");
    let dek = get_or_create_dek(
        &pool,
        &vault,
        &KeyCache::new(
            std::num::NonZeroUsize::new(1).unwrap(),
            std::time::Duration::from_secs(60),
        ),
        &format!("transit/{}", fixture.tenant_slug),
        customer_id,
    )
    .await
    .expect("fetching the customer's DEK for verification failed");
    let plaintext = encryption::decrypt(&dek, comms_request_id.as_bytes(), &ciphertext)
        .expect("decrypting the promoted ciphertext failed");
    let decoded: serde_json::Value = serde_json::from_slice(&plaintext)
        .expect("decrypted payload must be valid JSON");
    let expected: serde_json::Value =
        serde_json::from_slice(&body).expect("re-parsing the original body failed");
    assert_eq!(decoded, expected);

    assert_eq!(final_status.as_deref(), Some("delivered"));

    teardown(fixture).await;
}

// Scenario 2: the same, but with a provider_ref not yet known -> the row
// lands in orphan_event instead, unencrypted, after webhook-promote runs.
#[tokio::test]
async fn signed_receipt_with_no_match_lands_in_orphan_event() {
    let fixture = setup().await;
    let pool = fixture.tenant_pool().await;
    let vault = vault_keystore();

    let occurred_at = Utc::now();
    let body = receipt_body("never-matches", "delivered", occurred_at);
    let signature = sign(&fixture.webhook_secret, &body);

    let response = fixture
        .client()
        .post(fixture.url("mock-provider"))
        .header("x-webhook-signature", signature)
        .body(body.clone())
        .send()
        .await
        .expect("sending the signed webhook request failed");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(staging_count(&pool).await, 1);

    let report = run_webhook_promote(
        &fixture.control_pool,
        &fixture.control_url,
        &fixture.tenant_slug,
        &vault,
        5,
    )
    .await
    .expect("webhook promote run failed");
    assert_eq!(report.promoted, 0);
    assert_eq!(report.orphaned, 1);
    assert_eq!(staging_count(&pool).await, 0);

    let (provider_payload_raw, provider_ref): (serde_json::Value, String) = sqlx::query_as(
        "SELECT provider_payload_raw, provider_ref FROM orphan_event WHERE provider_ref = $1",
    )
    .bind("never-matches")
    .fetch_one(&pool)
    .await
    .expect("fetching the orphaned event failed");
    assert_eq!(provider_ref, "never-matches");
    let expected: serde_json::Value =
        serde_json::from_slice(&body).expect("re-parsing the original body failed");
    assert_eq!(provider_payload_raw, expected);

    teardown(fixture).await;
}

// Scenario 3: an unsigned or mis-signed receipt -> 401, no
// webhook_receipt_staging row written.
#[tokio::test]
async fn unsigned_or_mis_signed_receipts_are_rejected() {
    let fixture = setup().await;
    let pool = fixture.tenant_pool().await;

    let body = receipt_body("abc", "delivered", Utc::now());

    let unsigned = fixture
        .client()
        .post(fixture.url("mock-provider"))
        .body(body.clone())
        .send()
        .await
        .expect("sending the unsigned webhook request failed");
    assert_eq!(unsigned.status(), reqwest::StatusCode::UNAUTHORIZED);

    let mis_signed = fixture
        .client()
        .post(fixture.url("mock-provider"))
        .header("x-webhook-signature", sign(b"wrong-secret", &body))
        .body(body.clone())
        .send()
        .await
        .expect("sending the mis-signed webhook request failed");
    assert_eq!(mis_signed.status(), reqwest::StatusCode::UNAUTHORIZED);

    assert_eq!(staging_count(&pool).await, 0);

    teardown(fixture).await;
}

// Scenario 5 (T-047 rework, review F1): a signed bounce receipt promotes
// into comms_event same as any other, but also feeds the suppression list
// automatically -- the gap the review found, since only a manual
// `messgr-control suppression add` populated it before this fix.
#[tokio::test]
async fn a_bounce_receipt_auto_suppresses_the_destination() {
    let fixture = setup().await;
    let pool = fixture.tenant_pool().await;
    let vault = vault_keystore();

    let comms_request_id = Uuid::new_v4();
    let customer_id = Uuid::new_v4();
    let created_at = Utc::now();
    let occurred_at = Utc::now();
    insert_comms_request(
        &pool,
        comms_request_id,
        customer_id,
        created_at,
        Some("sent"),
    )
    .await;
    insert_comms_event(&pool, comms_request_id, customer_id, occurred_at, "abc").await;

    let body = receipt_body("abc", "bounced", occurred_at);
    let signature = sign(&fixture.webhook_secret, &body);

    let response = fixture
        .client()
        .post(fixture.url("mock-provider"))
        .header("x-webhook-signature", signature)
        .body(body)
        .send()
        .await
        .expect("sending the signed webhook request failed");
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    run_webhook_promote(
        &fixture.control_pool,
        &fixture.control_url,
        &fixture.tenant_slug,
        &vault,
        5,
    )
    .await
    .expect("webhook promote run failed");

    let (reason, review_at): (String, chrono::DateTime<Utc>) = sqlx::query_as(
        "SELECT reason, review_at FROM suppression WHERE destination_hmac = $1",
    )
    .bind(b"hmac".to_vec())
    .fetch_one(&pool)
    .await
    .expect("fetching the auto-created suppression row failed");
    assert_eq!(reason, "hard_bounce");
    let expected_review_at = Utc::now() + chrono::Duration::days(365);
    assert!(
        (review_at - expected_review_at).num_minutes().abs() < 5,
        "review_at should land ~1 year out, got {review_at}"
    );

    teardown(fixture).await;
}

// Scenario 4: the same signed receipt posted twice -> only one
// webhook_receipt_staging row.
#[tokio::test]
async fn a_duplicate_signed_receipt_writes_only_one_staging_row() {
    let fixture = setup().await;
    let pool = fixture.tenant_pool().await;

    let occurred_at = Utc::now();
    let body = receipt_body("abc", "delivered", occurred_at);
    let signature = sign(&fixture.webhook_secret, &body);

    for _ in 0..2 {
        let response = fixture
            .client()
            .post(fixture.url("mock-provider"))
            .header("x-webhook-signature", signature.clone())
            .body(body.clone())
            .send()
            .await
            .expect("sending the signed webhook request failed");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
    }

    assert_eq!(staging_count(&pool).await, 1);

    teardown(fixture).await;
}

//! `messgr-ingest` integration suite (DESIGN.md §4.1–§4.3, §7, §11, T-011),
//! following `tests/producer.rs`'s conventions: real provisioning against
//! the local stack, no mocks. Drives the real accept path
//! (`messgr::mtls::ClientCertAcceptor`) with a real mTLS handshake via
//! `reqwest`, not a fake extension inserted by hand.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::routing::post;
use axum_server::Handle;
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use reqwest::{Certificate, Identity};
use sqlx::PgPool;
use sqlx::postgres::types::PgInterval;
use uuid::Uuid;

use messgr::customer_dek::lifecycle::get_or_create_dek;
use messgr::db;
use messgr::encryption;
use messgr::ingest::AppState;
use messgr::ingest::handler::create_comms;
use messgr::key_cache::KeyCache;
use messgr::keystore::{KeyStore, VaultKeyStore};
use messgr::mtls::{self, ClientCertAcceptor};
use messgr::producer::cert_repo;
use messgr::producer::dev_pki;
use messgr::producer::register::{disable_producer, register_producer};
use messgr::profile::Profile;
use messgr::template::approve::approve_template;
use messgr::tenant::pool::connect_tenant_pool;
use messgr::tenant::provision::provision_tenant;
use messgr::tenant::registry::TenantRegistry;
use messgr::tenant_config::configure::set_tenant_config;
use messgr::tenant_config::model::{TenantConfigInput, verification_mode};

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

/// Same uniqueness as `unique_name`, but hyphen-separated — Vault PKI
/// validates a SAN as an actual DNS name (unlike the CN, which
/// `allow_any_name` lets through unchecked), and `_` is not a legal DNS
/// label character.
fn unique_hostname(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

fn sample_tenant_config(locale: &str) -> TenantConfigInput {
    TenantConfigInput {
        retention_years: 7,
        default_timezone: "UTC".to_string(),
        default_locale: locale.to_string(),
        schedule_horizon_days: 90,
        quota_day_boundary_tz: "UTC".to_string(),
        verification_mode: verification_mode::OBSERVE.to_string(),
        staleness_max_age: PgInterval {
            months: 0,
            days: 0,
            microseconds: 7_200 * 1_000_000,
        },
        kill_switch_release_rate: 500,
    }
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

async fn cleanup_cert(control_pool: &PgPool, cert_subject: &str) {
    let _ = cert_repo::delete_producer_cert(control_pool, cert_subject).await;
}

async fn teardown(fixture: &Fixture, cert_subject: Option<&str>) {
    fixture.server.shutdown();
    if let Some(cert_subject) = cert_subject {
        cleanup_cert(&fixture.control_pool, cert_subject).await;
    }
    let _ = std::fs::remove_dir_all(&fixture.cert_dir);
    drop_test_tenant(
        &fixture.control_pool,
        &fixture.database_name,
        &fixture.tenant_slug,
    )
    .await;
}

/// A running `messgr-ingest` router, wired exactly like `bin/ingest.rs`, on
/// an ephemeral port. `client_ca_pem`/leaf certs come from the dev PKI
/// (`dev_pki::bootstrap`/`issue_cert`), never a fixture checked into the
/// repo.
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
        client_ca_pem: &str,
        cert_dir: &Path,
    ) -> Self {
        let cert_path = cert_dir.join("server-cert.pem");
        let key_path = cert_dir.join("server-key.pem");
        let ca_path = cert_dir.join("client-ca.pem");
        std::fs::write(&cert_path, server_cert_pem)
            .expect("writing server cert failed");
        std::fs::write(&key_path, server_key_pem).expect("writing server key failed");
        std::fs::write(&ca_path, client_ca_pem).expect("writing client CA failed");

        let keystore: Arc<dyn KeyStore> = Arc::new(vault_keystore());
        let app_state = AppState {
            control_pool,
            control_database_url: control_url,
            keystore,
            registry: Arc::new(TenantRegistry::new()),
            tenant_pool_max_connections: 5,
            profile: Profile::Dev,
        };
        let app: Router = Router::new()
            .route("/comms", post(create_comms))
            .with_state(app_state);

        let tls_config = mtls::load_server_config(
            cert_path.to_str().unwrap(),
            key_path.to_str().unwrap(),
            ca_path.to_str().unwrap(),
        )
        .expect("loading test server TLS config failed");
        let rustls_config = RustlsConfig::from_config(Arc::new(tls_config));
        let acceptor = ClientCertAcceptor::new(RustlsAcceptor::new(rustls_config));

        let handle = Handle::new();
        let bind_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server_handle = handle.clone();
        tokio::spawn(async move {
            axum_server::bind(bind_addr)
                .handle(server_handle)
                .acceptor(acceptor)
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

/// A `reqwest::Client` presenting `client_identity_pem` (cert+key
/// concatenated, `reqwest::Identity::from_pem`'s own required shape) and
/// trusting `server_ca_pem`, resolving `hostname` to the test server's
/// actual ephemeral address so real hostname + chain verification both run
/// (no `danger_accept_invalid_*` escape hatch).
fn mtls_client(
    hostname: &str,
    addr: SocketAddr,
    client_identity_pem: &str,
    server_ca_pem: &str,
) -> reqwest::Client {
    let identity = Identity::from_pem(client_identity_pem.as_bytes())
        .expect("building client identity failed");
    let ca_cert = Certificate::from_pem(server_ca_pem.as_bytes())
        .expect("parsing server CA failed");

    reqwest::Client::builder()
        .identity(identity)
        .add_root_certificate(ca_cert)
        .resolve(hostname, addr)
        .build()
        .expect("building mTLS reqwest client failed")
}

struct Fixture {
    control_pool: PgPool,
    control_url: String,
    tenant_id: Uuid,
    tenant_slug: String,
    database_name: String,
    server_common_name: String,
    server: TestServer,
    server_ca_pem: String,
    cert_dir: std::path::PathBuf,
}

/// Provisions a tenant, sets its config, approves one template, bootstraps
/// dev PKI, mints a server certificate, and starts a `TestServer` — the
/// setup every test in this file shares. Producer registration/cert
/// minting is left to each test, since who gets to call in is exactly what
/// several of them vary.
async fn setup(locale: &str) -> Fixture {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("connecting to control database failed");
    let vault = vault_keystore();

    let tenant_slug = unique_name("test_tenant_ingest");
    let database_name = unique_name("test_db_ingest");

    let provision_outcome = provision_tenant(
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

    set_tenant_config(
        &control_pool,
        &control_url,
        &tenant_slug,
        sample_tenant_config(locale),
        "test-actor",
    )
    .await
    .expect("setting tenant config failed");

    approve_template(
        &control_pool,
        &control_url,
        &tenant_slug,
        "balance-alert",
        1,
        "sms",
        locale,
        "Hi {{name}}, your balance is {{balance}}.",
        "test-actor",
    )
    .await
    .expect("approving template failed");

    dev_pki::bootstrap(vault.client(), Profile::Dev)
        .await
        .expect("bootstrapping dev PKI failed");

    let server_common_name = unique_hostname("messgr-ingest.test");
    let server_cert =
        dev_pki::issue_server_cert(vault.client(), Profile::Dev, &server_common_name)
            .await
            .expect("issuing server certificate failed");

    let cert_dir = std::env::temp_dir().join(unique_name("messgr-ingest-test"));
    std::fs::create_dir_all(&cert_dir).expect("creating temp cert dir failed");

    let server = TestServer::start(
        control_pool.clone(),
        control_url.clone(),
        &server_cert.certificate,
        &server_cert.private_key,
        &server_cert.issuing_ca,
        &cert_dir,
    )
    .await;

    Fixture {
        control_pool,
        control_url,
        tenant_id: provision_outcome.tenant_id,
        tenant_slug,
        database_name,
        server_common_name,
        server,
        server_ca_pem: server_cert.issuing_ca,
        cert_dir,
    }
}

/// Registers a producer and issues it a client certificate. Returns the
/// certificate's PEM materials for building an `mtls_client`.
async fn register_test_producer(
    fixture: &Fixture,
    vault: &VaultKeyStore,
    name: &str,
) -> (String, String) {
    let common_name = unique_name(name);
    let cert_subject = format!("CN={common_name}");

    register_producer(
        &fixture.control_pool,
        &fixture.control_url,
        &fixture.tenant_slug,
        name,
        &cert_subject,
        "test-team",
        "oncall@example.com",
        "test-actor",
    )
    .await
    .expect("registering producer failed");

    let cert = dev_pki::issue_cert(vault.client(), Profile::Dev, &common_name)
        .await
        .expect("issuing client certificate failed");

    let identity_pem = format!("{}\n{}", cert.certificate, cert.private_key);
    (identity_pem, cert_subject)
}

fn sample_body(customer_id: Uuid) -> serde_json::Value {
    serde_json::json!({
        "customer_id": customer_id,
        "destination": "+15550100",
        "channel": "sms",
        "class": "transactional",
        "template_id": "balance-alert",
        "template_version": 1,
        "variables": { "name": "Jordan", "balance": "100.00" }
    })
}

fn sample_body_external(
    system: &str,
    external_id: &str,
    destination: &str,
) -> serde_json::Value {
    serde_json::json!({
        "external_id": external_id,
        "external_id_system": system,
        "destination": destination,
        "channel": "sms",
        "class": "transactional",
        "template_id": "balance-alert",
        "template_version": 1,
        "variables": { "name": "Jordan", "balance": "100.00" }
    })
}

fn sample_body_address_only(destination: &str) -> serde_json::Value {
    serde_json::json!({
        "destination": destination,
        "channel": "sms",
        "class": "transactional",
        "template_id": "balance-alert",
        "template_version": 1,
        "variables": { "name": "Jordan", "balance": "100.00" }
    })
}

#[tokio::test]
async fn create_comms_writes_ledger_and_outbox_and_idempotency_replays() {
    let fixture = setup("en-US").await;
    let vault = vault_keystore();
    let (identity_pem, _cert_subject) =
        register_test_producer(&fixture, &vault, "fraud-alerts").await;
    let client = mtls_client(
        &fixture.server_common_name,
        fixture.server.addr,
        &identity_pem,
        &fixture.server_ca_pem,
    );
    let url = format!(
        "https://{}:{}/comms",
        fixture.server_common_name,
        fixture.server.addr.port()
    );

    let customer_id = Uuid::new_v4();
    let idempotency_key = unique_name("idem");

    let response = client
        .post(&url)
        .header("Idempotency-Key", &idempotency_key)
        .json(&sample_body(customer_id))
        .send()
        .await
        .expect("request failed");
    assert_eq!(response.status(), 201, "expected 201 Created");
    let body: serde_json::Value =
        response.json().await.expect("parsing response failed");
    let comms_request_id: Uuid = body["comms_request_id"]
        .as_str()
        .expect("comms_request_id must be a string")
        .parse()
        .expect("comms_request_id must be a uuid");

    let tenant_pool = connect_tenant_pool(
        &fixture.control_pool,
        &fixture.control_url,
        fixture.tenant_id,
        &fixture.database_name,
        5,
    )
    .await
    .expect("connecting to tenant pool failed")
    .pool;

    let (channel, class, destination_hmac, destination_ciphertext, payload_ciphertext): (
        String,
        String,
        Vec<u8>,
        Vec<u8>,
        Option<Vec<u8>>,
    ) = sqlx::query_as(
        "SELECT channel, class, destination_hmac, destination_ciphertext, payload_ciphertext \
         FROM comms_request WHERE id = $1",
    )
    .bind(comms_request_id)
    .fetch_one(&tenant_pool)
    .await
    .expect("ledger row must exist");
    assert_eq!(channel, "sms");
    assert_eq!(class, "transactional");
    assert!(!destination_hmac.is_empty());

    let outbox_priority: i16 =
        sqlx::query_scalar("SELECT priority FROM outbox WHERE comms_request_id = $1")
            .bind(comms_request_id)
            .fetch_one(&tenant_pool)
            .await
            .expect("outbox row must exist");
    assert_eq!(outbox_priority, 1, "transactional must be priority 1");

    let dek_cache = KeyCache::new(
        std::num::NonZeroUsize::new(4).unwrap(),
        std::time::Duration::from_secs(60),
    );
    let dek = get_or_create_dek(
        &tenant_pool,
        &vault,
        &dek_cache,
        &format!("transit/{}", fixture.tenant_slug),
        customer_id,
    )
    .await
    .expect("fetching DEK failed");

    let aad = comms_request_id.as_bytes();
    let destination = encryption::decrypt(&dek, aad, &destination_ciphertext)
        .expect("decrypting destination failed");
    assert_eq!(destination, b"+15550100");
    let payload = encryption::decrypt(
        &dek,
        aad,
        &payload_ciphertext.expect("payload must be present"),
    )
    .expect("decrypting payload failed");
    assert_eq!(payload, b"Hi Jordan, your balance is 100.00.");

    // Idempotency replay: same key, same body -> 200, same id, no new row.
    let replay = client
        .post(&url)
        .header("Idempotency-Key", &idempotency_key)
        .json(&sample_body(customer_id))
        .send()
        .await
        .expect("replay request failed");
    assert_eq!(replay.status(), 200, "replay must be 200, not 201");
    let replay_body: serde_json::Value =
        replay.json().await.expect("parsing replay response failed");
    assert_eq!(replay_body["comms_request_id"], body["comms_request_id"]);

    let row_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM comms_request WHERE id = $1")
            .bind(comms_request_id)
            .fetch_one(&tenant_pool)
            .await
            .expect("counting ledger rows failed");
    assert_eq!(row_count, 1, "replay must not write a second ledger row");

    tenant_pool.close().await;
    teardown(&fixture, Some(&_cert_subject)).await;
}

#[tokio::test]
async fn concurrent_identical_requests_do_not_double_send() {
    let fixture = setup("en-US").await;
    let vault = vault_keystore();
    let (identity_pem, _cert_subject) =
        register_test_producer(&fixture, &vault, "concurrent-caller").await;
    let client = mtls_client(
        &fixture.server_common_name,
        fixture.server.addr,
        &identity_pem,
        &fixture.server_ca_pem,
    );
    let url = format!(
        "https://{}:{}/comms",
        fixture.server_common_name,
        fixture.server.addr.port()
    );

    let customer_id = Uuid::new_v4();
    let idempotency_key = unique_name("idem");
    let body = sample_body(customer_id);

    // Two requests with the same Idempotency-Key, fired at the same time —
    // this is the actual race the claim-then-insert transaction
    // (`ingest::repo::insert_transactional`) has to close: two overlapping
    // POSTs must never both win the claim, since that would double-send.
    let request_one = client
        .post(&url)
        .header("Idempotency-Key", &idempotency_key)
        .json(&body)
        .send();
    let request_two = client
        .post(&url)
        .header("Idempotency-Key", &idempotency_key)
        .json(&body)
        .send();
    let (response_one, response_two) = tokio::join!(request_one, request_two);
    let response_one = response_one.expect("first concurrent request failed");
    let response_two = response_two.expect("second concurrent request failed");

    let statuses = [response_one.status(), response_two.status()];
    assert!(
        statuses.contains(&reqwest::StatusCode::CREATED),
        "exactly one of the two concurrent requests must observe 201: got {statuses:?}"
    );
    assert!(
        statuses.contains(&reqwest::StatusCode::OK),
        "exactly one of the two concurrent requests must observe 200 (lost the claim): got {statuses:?}"
    );

    let body_one: serde_json::Value =
        response_one.json().await.expect("parsing response failed");
    let body_two: serde_json::Value =
        response_two.json().await.expect("parsing response failed");
    assert_eq!(
        body_one["comms_request_id"], body_two["comms_request_id"],
        "both concurrent requests must resolve to the same comms_request_id"
    );

    let tenant_pool = connect_tenant_pool(
        &fixture.control_pool,
        &fixture.control_url,
        fixture.tenant_id,
        &fixture.database_name,
        5,
    )
    .await
    .expect("connecting to tenant pool failed")
    .pool;
    let row_count: i64 = sqlx::query_scalar("SELECT count(*) FROM comms_request")
        .fetch_one(&tenant_pool)
        .await
        .expect("counting ledger rows failed");
    assert_eq!(
        row_count, 1,
        "concurrent identical requests must write exactly one ledger row"
    );
    let outbox_count: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox")
        .fetch_one(&tenant_pool)
        .await
        .expect("counting outbox rows failed");
    assert_eq!(
        outbox_count, 1,
        "concurrent identical requests must write exactly one outbox row"
    );
    tenant_pool.close().await;

    teardown(&fixture, Some(&_cert_subject)).await;
}

#[tokio::test]
async fn unregistered_client_certificate_is_rejected() {
    let fixture = setup("en-US").await;
    let vault = vault_keystore();

    // A certificate the dev CA happily signs, but never registered as a
    // producer_cert row — the TLS handshake succeeds (trusted CA), proving
    // the chain alone is not authorization.
    let common_name = unique_name("unregistered-caller");
    let cert = dev_pki::issue_cert(vault.client(), Profile::Dev, &common_name)
        .await
        .expect("issuing certificate failed");
    let identity_pem = format!("{}\n{}", cert.certificate, cert.private_key);

    let client = mtls_client(
        &fixture.server_common_name,
        fixture.server.addr,
        &identity_pem,
        &fixture.server_ca_pem,
    );
    let url = format!(
        "https://{}:{}/comms",
        fixture.server_common_name,
        fixture.server.addr.port()
    );

    let response = client
        .post(&url)
        .header("Idempotency-Key", unique_name("idem"))
        .json(&sample_body(Uuid::new_v4()))
        .send()
        .await
        .expect("request failed");
    assert_eq!(response.status(), 403);
    let body: serde_json::Value =
        response.json().await.expect("parsing response failed");
    assert_eq!(body["error"], "unregistered producer certificate");

    teardown(&fixture, None).await;
}

#[tokio::test]
async fn disabled_producer_certificate_is_rejected() {
    let fixture = setup("en-US").await;
    let vault = vault_keystore();
    let (identity_pem, _cert_subject) =
        register_test_producer(&fixture, &vault, "disabled-caller").await;

    disable_producer(
        &fixture.control_pool,
        &fixture.control_url,
        &fixture.tenant_slug,
        "disabled-caller",
        "test-actor",
    )
    .await
    .expect("disabling producer failed");

    let client = mtls_client(
        &fixture.server_common_name,
        fixture.server.addr,
        &identity_pem,
        &fixture.server_ca_pem,
    );
    let url = format!(
        "https://{}:{}/comms",
        fixture.server_common_name,
        fixture.server.addr.port()
    );

    let response = client
        .post(&url)
        .header("Idempotency-Key", unique_name("idem"))
        .json(&sample_body(Uuid::new_v4()))
        .send()
        .await
        .expect("request failed");
    assert_eq!(response.status(), 403);
    let body: serde_json::Value =
        response.json().await.expect("parsing response failed");
    assert_eq!(body["error"], "producer is disabled");

    teardown(&fixture, Some(&_cert_subject)).await;
}

/// T-016, closing T-011/F3: a suspended tenant's still-enabled producer cert
/// must be rejected too, distinctly from `disabled_producer_certificate_is_rejected`
/// above. No CLI/repo path to suspend a tenant exists yet (§7.7 offboarding
/// enforcement is step 19) -- the raw `UPDATE` below stands in for that.
#[tokio::test]
async fn suspended_tenant_producer_is_rejected() {
    let fixture = setup("en-US").await;
    let vault = vault_keystore();
    let (identity_pem, _cert_subject) =
        register_test_producer(&fixture, &vault, "suspended-tenant-caller").await;

    sqlx::query("UPDATE tenant SET status = 'suspended' WHERE slug = $1")
        .bind(&fixture.tenant_slug)
        .execute(&fixture.control_pool)
        .await
        .expect("suspending the tenant failed");

    let client = mtls_client(
        &fixture.server_common_name,
        fixture.server.addr,
        &identity_pem,
        &fixture.server_ca_pem,
    );
    let url = format!(
        "https://{}:{}/comms",
        fixture.server_common_name,
        fixture.server.addr.port()
    );

    let response = client
        .post(&url)
        .header("Idempotency-Key", unique_name("idem"))
        .json(&sample_body(Uuid::new_v4()))
        .send()
        .await
        .expect("request failed");
    assert_eq!(response.status(), 403);
    let body: serde_json::Value =
        response.json().await.expect("parsing response failed");
    assert_eq!(body["error"], "tenant is not active");

    teardown(&fixture, Some(&_cert_subject)).await;
}

#[tokio::test]
async fn request_validation_rejects_bad_input_before_any_write() {
    let fixture = setup("en-US").await;
    let vault = vault_keystore();
    let (identity_pem, _cert_subject) =
        register_test_producer(&fixture, &vault, "validation-caller").await;
    let client = mtls_client(
        &fixture.server_common_name,
        fixture.server.addr,
        &identity_pem,
        &fixture.server_ca_pem,
    );
    let url = format!(
        "https://{}:{}/comms",
        fixture.server_common_name,
        fixture.server.addr.port()
    );

    // class = "auth" is a hard 422 — auth/OTP never goes through this endpoint.
    let mut auth_body = sample_body(Uuid::new_v4());
    auth_body["class"] = serde_json::json!("auth");
    let response = client
        .post(&url)
        .header("Idempotency-Key", unique_name("idem"))
        .json(&auth_body)
        .send()
        .await
        .expect("request failed");
    assert_eq!(response.status(), 422);

    // campaign_id must be null for class = "transactional".
    let mut campaign_body = sample_body(Uuid::new_v4());
    campaign_body["campaign_id"] = serde_json::json!("summer-sale");
    let response = client
        .post(&url)
        .header("Idempotency-Key", unique_name("idem"))
        .json(&campaign_body)
        .send()
        .await
        .expect("request failed");
    assert_eq!(response.status(), 422);

    // Unknown channel.
    let mut bad_channel_body = sample_body(Uuid::new_v4());
    bad_channel_body["channel"] = serde_json::json!("carrier-pigeon");
    let response = client
        .post(&url)
        .header("Idempotency-Key", unique_name("idem"))
        .json(&bad_channel_body)
        .send()
        .await
        .expect("request failed");
    assert_eq!(response.status(), 422);

    // Missing Idempotency-Key header.
    let response = client
        .post(&url)
        .json(&sample_body(Uuid::new_v4()))
        .send()
        .await
        .expect("request failed");
    assert_eq!(response.status(), 400);

    // Unknown template.
    let mut bad_template_body = sample_body(Uuid::new_v4());
    bad_template_body["template_id"] = serde_json::json!("does-not-exist");
    let response = client
        .post(&url)
        .header("Idempotency-Key", unique_name("idem"))
        .json(&bad_template_body)
        .send()
        .await
        .expect("request failed");
    assert_eq!(response.status(), 404);

    let tenant_pool = connect_tenant_pool(
        &fixture.control_pool,
        &fixture.control_url,
        fixture.tenant_id,
        &fixture.database_name,
        5,
    )
    .await
    .expect("connecting to tenant pool failed")
    .pool;
    let row_count: i64 = sqlx::query_scalar("SELECT count(*) FROM comms_request")
        .fetch_one(&tenant_pool)
        .await
        .expect("counting ledger rows failed");
    assert_eq!(
        row_count, 0,
        "no rejected request should write a ledger row"
    );
    tenant_pool.close().await;

    teardown(&fixture, Some(&_cert_subject)).await;
}

#[tokio::test]
async fn resolved_address_id_is_a_real_customer_address_row() {
    let fixture = setup("en-US").await;
    let vault = vault_keystore();
    let (identity_pem, _cert_subject) =
        register_test_producer(&fixture, &vault, "address-row-caller").await;
    let client = mtls_client(
        &fixture.server_common_name,
        fixture.server.addr,
        &identity_pem,
        &fixture.server_ca_pem,
    );
    let url = format!(
        "https://{}:{}/comms",
        fixture.server_common_name,
        fixture.server.addr.port()
    );

    let customer_id = Uuid::new_v4();
    let response = client
        .post(&url)
        .header("Idempotency-Key", unique_name("idem"))
        .json(&sample_body(customer_id))
        .send()
        .await
        .expect("request failed");
    assert_eq!(response.status(), 201);
    let body: serde_json::Value =
        response.json().await.expect("parsing response failed");
    let comms_request_id: Uuid = body["comms_request_id"]
        .as_str()
        .expect("comms_request_id must be a string")
        .parse()
        .expect("comms_request_id must be a uuid");

    let tenant_pool = connect_tenant_pool(
        &fixture.control_pool,
        &fixture.control_url,
        fixture.tenant_id,
        &fixture.database_name,
        5,
    )
    .await
    .expect("connecting to tenant pool failed")
    .pool;

    let address_id: Uuid =
        sqlx::query_scalar("SELECT address_id FROM outbox WHERE comms_request_id = $1")
            .bind(comms_request_id)
            .fetch_one(&tenant_pool)
            .await
            .expect("outbox row must exist");

    let address_customer_id: Uuid =
        sqlx::query_scalar("SELECT customer_id FROM customer_address WHERE id = $1")
            .bind(address_id)
            .fetch_one(&tenant_pool)
            .await
            .expect("outbox.address_id must reference a real customer_address row");
    assert_eq!(address_customer_id, customer_id);

    tenant_pool.close().await;
    teardown(&fixture, Some(&_cert_subject)).await;
}

#[tokio::test]
async fn address_only_request_without_customer_id_or_external_id_succeeds() {
    let fixture = setup("en-US").await;
    let vault = vault_keystore();
    let (identity_pem, _cert_subject) =
        register_test_producer(&fixture, &vault, "address-only-caller").await;
    let client = mtls_client(
        &fixture.server_common_name,
        fixture.server.addr,
        &identity_pem,
        &fixture.server_ca_pem,
    );
    let url = format!(
        "https://{}:{}/comms",
        fixture.server_common_name,
        fixture.server.addr.port()
    );

    let response = client
        .post(&url)
        .header("Idempotency-Key", unique_name("idem"))
        .json(&sample_body_address_only("+15550900"))
        .send()
        .await
        .expect("request failed");
    assert_eq!(response.status(), 201, "expected 201 Created");

    let tenant_pool = connect_tenant_pool(
        &fixture.control_pool,
        &fixture.control_url,
        fixture.tenant_id,
        &fixture.database_name,
        5,
    )
    .await
    .expect("connecting to tenant pool failed")
    .pool;
    let customer_count: i64 = sqlx::query_scalar("SELECT count(*) FROM customer")
        .fetch_one(&tenant_pool)
        .await
        .expect("counting customer rows failed");
    assert_eq!(
        customer_count, 1,
        "an address-only request must provision exactly one customer"
    );

    tenant_pool.close().await;
    teardown(&fixture, Some(&_cert_subject)).await;
}

#[tokio::test]
async fn customer_id_and_external_id_together_is_rejected() {
    let fixture = setup("en-US").await;
    let vault = vault_keystore();
    let (identity_pem, _cert_subject) =
        register_test_producer(&fixture, &vault, "both-ids-caller").await;
    let client = mtls_client(
        &fixture.server_common_name,
        fixture.server.addr,
        &identity_pem,
        &fixture.server_ca_pem,
    );
    let url = format!(
        "https://{}:{}/comms",
        fixture.server_common_name,
        fixture.server.addr.port()
    );

    let mut body = sample_body(Uuid::new_v4());
    body["external_id"] = serde_json::json!("cust-123");
    body["external_id_system"] = serde_json::json!("core_banking");

    let response = client
        .post(&url)
        .header("Idempotency-Key", unique_name("idem"))
        .json(&body)
        .send()
        .await
        .expect("request failed");
    assert_eq!(response.status(), 422);
    let response_body: serde_json::Value =
        response.json().await.expect("parsing response failed");
    assert_eq!(
        response_body["error"],
        "exactly one of customer_id, (external_id + external_id_system), or neither must be set"
    );

    teardown(&fixture, Some(&_cert_subject)).await;
}

#[tokio::test]
async fn external_id_request_resolves_to_the_same_customer_on_replay() {
    let fixture = setup("en-US").await;
    let vault = vault_keystore();
    let (identity_pem, _cert_subject) =
        register_test_producer(&fixture, &vault, "external-id-caller").await;
    let client = mtls_client(
        &fixture.server_common_name,
        fixture.server.addr,
        &identity_pem,
        &fixture.server_ca_pem,
    );
    let url = format!(
        "https://{}:{}/comms",
        fixture.server_common_name,
        fixture.server.addr.port()
    );

    let system = "core_banking";
    let external_id = unique_name("ext");

    let response = client
        .post(&url)
        .header("Idempotency-Key", unique_name("idem"))
        .json(&sample_body_external(system, &external_id, "+15551100"))
        .send()
        .await
        .expect("first request failed");
    assert_eq!(response.status(), 201);

    let tenant_pool = connect_tenant_pool(
        &fixture.control_pool,
        &fixture.control_url,
        fixture.tenant_id,
        &fixture.database_name,
        5,
    )
    .await
    .expect("connecting to tenant pool failed")
    .pool;
    let customer_id: Uuid = sqlx::query_scalar(
        "SELECT customer_id FROM customer_external_id WHERE system = $1 AND external_id = $2",
    )
    .bind(system)
    .bind(&external_id)
    .fetch_one(&tenant_pool)
    .await
    .expect("customer_external_id row must exist");

    // A second, distinct send for the same external id must resolve to the
    // same customer rather than minting a second one.
    let response = client
        .post(&url)
        .header("Idempotency-Key", unique_name("idem"))
        .json(&sample_body_external(system, &external_id, "+15551100"))
        .send()
        .await
        .expect("second request failed");
    assert_eq!(response.status(), 201);

    let customer_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM customer WHERE id = $1")
            .bind(customer_id)
            .fetch_one(&tenant_pool)
            .await
            .expect("counting customer rows failed");
    assert_eq!(customer_count, 1);

    tenant_pool.close().await;
    teardown(&fixture, Some(&_cert_subject)).await;
}

#[tokio::test]
async fn same_destination_under_two_customer_ids_resolves_to_the_first() {
    // Regression for T-018: this used to assert 409 — src/customer/resolve.rs
    // used to reject the second send with AddressConflict, contradicting
    // DESIGN.md §4.7's "never reject a send because resolution failed".
    let fixture = setup("en-US").await;
    let vault = vault_keystore();
    let (identity_pem, _cert_subject) =
        register_test_producer(&fixture, &vault, "conflict-caller").await;
    let client = mtls_client(
        &fixture.server_common_name,
        fixture.server.addr,
        &identity_pem,
        &fixture.server_ca_pem,
    );
    let url = format!(
        "https://{}:{}/comms",
        fixture.server_common_name,
        fixture.server.addr.port()
    );

    let destination = "+15551000";
    let customer_a = Uuid::new_v4();
    let mut first_body = sample_body(customer_a);
    first_body["destination"] = serde_json::json!(destination);
    let response = client
        .post(&url)
        .header("Idempotency-Key", unique_name("idem"))
        .json(&first_body)
        .send()
        .await
        .expect("first request failed");
    assert_eq!(response.status(), 201);

    let mut second_body = sample_body(Uuid::new_v4());
    second_body["destination"] = serde_json::json!(destination);
    let response = client
        .post(&url)
        .header("Idempotency-Key", unique_name("idem"))
        .json(&second_body)
        .send()
        .await
        .expect("second request failed");
    assert_eq!(
        response.status(),
        201,
        "resolution must not reject the send"
    );
    let body: serde_json::Value =
        response.json().await.expect("parsing response failed");
    let comms_request_id: Uuid = body["comms_request_id"]
        .as_str()
        .expect("comms_request_id must be a string")
        .parse()
        .expect("comms_request_id must be a uuid");

    let tenant_pool = connect_tenant_pool(
        &fixture.control_pool,
        &fixture.control_url,
        fixture.tenant_id,
        &fixture.database_name,
        5,
    )
    .await
    .expect("connecting to tenant pool failed")
    .pool;

    let resolved_customer_id: Uuid =
        sqlx::query_scalar("SELECT customer_id FROM comms_request WHERE id = $1")
            .bind(comms_request_id)
            .fetch_one(&tenant_pool)
            .await
            .expect("comms_request row must exist");
    assert_eq!(
        resolved_customer_id, customer_a,
        "the second send must resolve to the first (winning) customer, not its own asserted id"
    );

    tenant_pool.close().await;
    teardown(&fixture, Some(&_cert_subject)).await;
}

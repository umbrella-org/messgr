//! `messgr-sms-sender` acceptance suite (T-052), mirroring `tests/ingest.rs`'s
//! conventions: real provisioning against the local stack, no mocks except
//! the outbound SMS provider (`wiremock`, the same tool `tests/dispatcher.rs`
//! already uses to exercise `sender::http::HttpSender` without a real
//! vendor).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::routing::post;
use axum_server::Handle;
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use chrono::{DateTime, Utc};
use reqwest::{Certificate, Identity};
use sqlx::PgPool;
use uuid::Uuid;
use wiremock::matchers::{body_json, header, method, path as wpath};
use wiremock::{Mock, MockServer, ResponseTemplate};

use messgr::db;
use messgr::key_cache::KeyCache;
use messgr::keystore::{KeyStore, VaultKeyStore};
use messgr::kill_switch::cache::KillSwitchCache;
use messgr::mtls::{self, ClientCertAcceptor};
use messgr::producer::cert_repo;
use messgr::producer::dev_pki;
use messgr::producer::register::register_producer;
use messgr::profile::Profile;
use messgr::provider_config::configure::set_provider_config;
use messgr::provider_config::model::ProviderConfigInput;
use messgr::sms_sender::AppState;
use messgr::sms_sender::auth_flag::AuthEnabledCache;
use messgr::sms_sender::buffer;
use messgr::sms_sender::handler::send_otp;
use messgr::sms_sender::identity::ProducerContext;
use messgr::sms_sender::pending;
use messgr::sms_sender::provider::ProviderConfigCache;
use messgr::tenant::pool::connect_tenant_pool;
use messgr::tenant::provision::provision_tenant;
use messgr::tenant::registry::{TenantContext, TenantRegistry};
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
        kill_switch_release_rate: 500,
        reconcile_attempts_cap: 5,
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
    let _ = std::fs::remove_file(&fixture.buffer_path);
    let _ = std::fs::remove_file(&fixture.pending_path);
    drop_test_tenant(
        &fixture.control_pool,
        &fixture.database_name,
        &fixture.tenant_slug,
    )
    .await;
}

/// A running `messgr-sms-sender` router, wired exactly like `bin/sms_sender.rs`,
/// on an ephemeral port.
struct TestServer {
    addr: SocketAddr,
    handle: Handle<SocketAddr>,
}

impl TestServer {
    #[allow(clippy::too_many_arguments)]
    async fn start(
        control_pool: PgPool,
        control_url: String,
        server_cert_pem: &str,
        server_key_pem: &str,
        client_ca_pem: &str,
        cert_dir: &Path,
        auth_flag: Arc<AuthEnabledCache>,
        sms_base_url: String,
        buffer_path: PathBuf,
        pending_path: PathBuf,
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
            auth_flag,
            provider_config_cache: Arc::new(ProviderConfigCache::new()),
            sms_base_url,
            buffer_path,
            pending_path,
        };
        let app: Router = Router::new()
            .route("/otp", post(send_otp))
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
    buffer_path: PathBuf,
    pending_path: PathBuf,
}

/// Provisions a tenant, sets its config, bootstraps dev PKI, mints a server
/// certificate, and starts a `TestServer` pointed at `sms_base_url` --
/// `messgr-sms-sender` never renders or reads a template, so unlike
/// `tests/ingest.rs`'s fixture, nothing here approves one.
async fn setup(locale: &str, sms_base_url: &str) -> Fixture {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("connecting to control database failed");
    let vault = vault_keystore();

    let tenant_slug = unique_name("test_tenant_sms_sender");
    let database_name = unique_name("test_db_sms_sender");

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

    dev_pki::bootstrap(vault.client(), Profile::Dev)
        .await
        .expect("bootstrapping dev PKI failed");

    let server_common_name = unique_hostname("messgr-sms-sender.test");
    let server_cert =
        dev_pki::issue_server_cert(vault.client(), Profile::Dev, &server_common_name)
            .await
            .expect("issuing server certificate failed");

    let cert_dir = std::env::temp_dir().join(unique_name("messgr-sms-sender-test"));
    std::fs::create_dir_all(&cert_dir).expect("creating temp cert dir failed");
    let buffer_path = std::env::temp_dir()
        .join(unique_name("sms-sender-buffer"))
        .with_extension("jsonl");
    let pending_path = std::env::temp_dir()
        .join(unique_name("sms-sender-pending"))
        .with_extension("jsonl");

    let auth_flag = Arc::new(AuthEnabledCache::new());
    auth_flag
        .refresh(&control_pool)
        .await
        .expect("initial auth_enabled refresh failed");

    let server = TestServer::start(
        control_pool.clone(),
        control_url.clone(),
        &server_cert.certificate,
        &server_cert.private_key,
        &server_cert.issuing_ca,
        &cert_dir,
        auth_flag,
        sms_base_url.to_string(),
        buffer_path.clone(),
        pending_path.clone(),
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
        buffer_path,
        pending_path,
    }
}

/// Registers a producer and issues it a client certificate -- the bank's own
/// auth service, in this binary's terms (decision 2: an ordinary producer,
/// no new identity mechanism).
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

/// Sets one `provider_config` row and seeds its Vault KV credential --
/// `api_key` is what `sender::http::HttpSender`'s `bearer_auth` sends, and
/// what a mounted `wiremock` `Mock` matches on to tell providers apart.
async fn set_provider(
    fixture: &Fixture,
    vault: &VaultKeyStore,
    priority: i16,
    api_key: &str,
) {
    let kv_path = format!("{}/sms-priority-{priority}", fixture.tenant_slug);
    vaultrs::kv2::set(
        vault.client(),
        "secret",
        &kv_path,
        &serde_json::json!({"api_key": api_key}),
    )
    .await
    .expect("writing provider credential to Vault failed");

    set_provider_config(
        &fixture.control_pool,
        &fixture.control_url,
        &fixture.tenant_slug,
        ProviderConfigInput {
            channel: "sms".to_string(),
            priority,
            provider: "generic-http".to_string(),
            credential_path: format!("secret/data/{kv_path}"),
            rate_limit_per_sec: 100,
        },
        "test-actor",
    )
    .await
    .expect("setting provider_config failed");
}

fn otp_body(customer_id: Uuid, destination: &str) -> serde_json::Value {
    serde_json::json!({
        "customer_id": customer_id,
        "destination": destination,
        "body": "your code is 123456",
    })
}

async fn wait_for_comms_request(
    tenant_pool: &PgPool,
    comms_request_id: Uuid,
) -> (String, String, Option<String>) {
    for _ in 0..50 {
        let row: Option<(String, String, Option<String>)> = sqlx::query_as(
            "SELECT final_status, class, payload_ciphertext::text FROM comms_request \
             WHERE id = $1 AND final_status IS NOT NULL",
        )
        .bind(comms_request_id)
        .fetch_optional(tenant_pool)
        .await
        .expect("querying comms_request failed");
        if let Some(row) = row {
            return row;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("comms_request row for {comms_request_id} never appeared");
}

async fn fetch_timestamps(
    tenant_pool: &PgPool,
    comms_request_id: Uuid,
) -> (DateTime<Utc>, DateTime<Utc>) {
    sqlx::query_as("SELECT created_at, finalized_at FROM comms_request WHERE id = $1")
        .bind(comms_request_id)
        .fetch_one(tenant_pool)
        .await
        .expect("querying comms_request timestamps failed")
}

#[tokio::test]
async fn send_otp_writes_ledger_and_calls_provider_exactly_once() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wpath("/messages"))
        .and(header("Authorization", "Bearer test-key-1"))
        .and(body_json(serde_json::json!({
            "to": "+15550100",
            "body": "your code is 123456",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "msg-1",
            "status": "queued",
        })))
        .mount(&mock_server)
        .await;

    let fixture = setup("en-US", &mock_server.uri()).await;
    let vault = vault_keystore();
    set_provider(&fixture, &vault, 1, "test-key-1").await;

    let (identity_pem, cert_subject) =
        register_test_producer(&fixture, &vault, "auth-service").await;
    let client = mtls_client(
        &fixture.server_common_name,
        fixture.server.addr,
        &identity_pem,
        &fixture.server_ca_pem,
    );
    let url = format!(
        "https://{}:{}/otp",
        fixture.server_common_name,
        fixture.server.addr.port()
    );

    let customer_id = Uuid::new_v4();
    let response = client
        .post(&url)
        .json(&otp_body(customer_id, "+15550100"))
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

    assert_eq!(
        mock_server.received_requests().await.unwrap().len(),
        1,
        "the provider must receive exactly one call"
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

    let (final_status, class, payload_ciphertext) =
        wait_for_comms_request(&tenant_pool, comms_request_id).await;
    assert_eq!(final_status, "sent");
    assert_eq!(class, "auth");
    assert_eq!(
        payload_ciphertext, None,
        "payload_ciphertext must stay NULL for the auth class"
    );

    // F2 regression: `finalized_at` used to be bound to the same query
    // parameter as `created_at`, so every row landed with the two equal.
    let (created_at, finalized_at) =
        fetch_timestamps(&tenant_pool, comms_request_id).await;
    assert_ne!(
        finalized_at, created_at,
        "finalized_at must be the actual write time, not a copy of created_at (F2)"
    );
    assert!(
        finalized_at >= created_at,
        "finalized_at must not predate created_at"
    );

    tenant_pool.close().await;
    teardown(&fixture, Some(&cert_subject)).await;
}

#[tokio::test]
async fn postgres_write_failure_buffers_and_drain_recovers_it() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wpath("/messages"))
        .and(header("Authorization", "Bearer test-key-buffer"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "msg-buffer",
            "status": "queued",
        })))
        .mount(&mock_server)
        .await;

    let fixture = setup("en-US", &mock_server.uri()).await;
    let vault = vault_keystore();
    set_provider(&fixture, &vault, 1, "test-key-buffer").await;

    let registry = TenantRegistry::new();
    let keystore: Arc<dyn KeyStore> = Arc::new(vault_keystore());
    let tenant = registry
        .get_or_open(
            &fixture.control_pool,
            &fixture.control_url,
            keystore.as_ref(),
            fixture.tenant_id,
            5,
        )
        .await
        .expect("opening tenant context failed");

    // A pool pointed at the same tenant database, then closed -- the
    // precedented way this codebase's own tests simulate "Postgres
    // unreachable" for one specific pool without tearing down the real
    // database (see e.g. `tests/consent.rs`'s `pool.close()` usage).
    let broken_pool = connect_tenant_pool(
        &fixture.control_pool,
        &fixture.control_url,
        fixture.tenant_id,
        &fixture.database_name,
        2,
    )
    .await
    .expect("opening the pool to break failed")
    .pool;
    broken_pool.close().await;

    // The DEK is pre-warmed into the cache rather than fetched through the
    // (now-broken) pool -- `get_or_create_dek`'s own cache check runs before
    // any DB/Vault call, so this proves the audit write (not DEK
    // resolution) is what this test breaks, matching decision 9's framing:
    // "the OTP send already completed one way or the other" by the time the
    // write is attempted.
    let customer_id = Uuid::new_v4();
    let dek_cache = KeyCache::new(
        std::num::NonZeroUsize::new(4).unwrap(),
        std::time::Duration::from_secs(60),
    );
    dek_cache.put(customer_id, zeroize::Zeroizing::new(vec![0x42; 32]));

    let broken_tenant = Arc::new(TenantContext {
        tenant: tenant.tenant.clone(),
        pool: broken_pool,
        config: tenant.config.clone(),
        pepper: tenant.pepper.clone(),
        dek_cache,
        kill_switches: Arc::new(KillSwitchCache::new()),
    });

    let app_state = AppState {
        control_pool: fixture.control_pool.clone(),
        control_database_url: fixture.control_url.clone(),
        keystore: keystore.clone(),
        registry: Arc::new(registry),
        tenant_pool_max_connections: 5,
        profile: Profile::Dev,
        auth_flag: Arc::new(AuthEnabledCache::new()),
        provider_config_cache: Arc::new(ProviderConfigCache::new()),
        sms_base_url: mock_server.uri(),
        buffer_path: fixture.buffer_path.clone(),
        pending_path: fixture.pending_path.clone(),
    };
    app_state
        .auth_flag
        .refresh(&fixture.control_pool)
        .await
        .expect("auth_enabled refresh failed");
    // Primed against the still-working pool, before `tenant.pool` is
    // replaced below -- proves provider *selection* survives the same
    // outage that forces the audit write to buffer (see
    // `ProviderConfigCache`'s own doc comment).
    app_state
        .provider_config_cache
        .load(&tenant.pool, fixture.tenant_id)
        .await;

    let (status, response) = send_otp(
        State(app_state.clone()),
        ProducerContext {
            producer_id: Uuid::new_v4(),
            tenant: broken_tenant,
        },
        axum::Json(serde_json::from_value(otp_body(customer_id, "+15550200")).unwrap()),
    )
    .await
    .expect("handler must still return success -- the provider call is unaffected");

    assert_eq!(status, axum::http::StatusCode::CREATED);
    let comms_request_id = response.0.comms_request_id;

    let buffered = std::fs::read_to_string(&fixture.buffer_path)
        .expect("the buffer file must exist after a failed write");
    assert!(
        buffered.contains(&comms_request_id.to_string()),
        "the buffered record must carry this send's comms_request_id"
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
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM comms_request")
        .fetch_one(&tenant_pool)
        .await
        .expect("counting comms_request rows failed");
    assert_eq!(before, 0, "the row must not exist before draining");

    buffer::drain(
        &fixture.buffer_path,
        &fixture.control_pool,
        &fixture.control_url,
        keystore.as_ref(),
        &app_state.registry,
        5,
    )
    .await;

    let (final_status, _, _) =
        wait_for_comms_request(&tenant_pool, comms_request_id).await;
    assert_eq!(final_status, "sent", "the drained row must reach sent");

    let remaining = std::fs::read_to_string(&fixture.buffer_path).unwrap_or_default();
    assert!(
        remaining.trim().is_empty(),
        "a successfully drained record must be removed from the buffer file"
    );

    tenant_pool.close().await;
    teardown(&fixture, None).await;
}

/// Exercises the handler function directly rather than through the full
/// mTLS `TestServer` -- `AuthEnabledCache` refreshes on a 5s poll (decision
/// 8), and waiting out a real interval to prove this deterministically
/// would make the test slow and timing-dependent for no extra coverage: the
/// cache's own read (`is_enabled`) and the handler's branch on it are what
/// this proves, not the poll timer itself.
#[tokio::test]
async fn auth_disabled_tenant_never_calls_provider() {
    let mock_server = MockServer::start().await;
    // No Mock mounted at all -- any call the handler made to this server
    // would fail to match and the test would fail on the assertion below,
    // not silently pass.

    let fixture = setup("en-US", &mock_server.uri()).await;

    sqlx::query("UPDATE tenant SET auth_enabled = false WHERE id = $1")
        .bind(fixture.tenant_id)
        .execute(&fixture.control_pool)
        .await
        .expect("disabling auth_enabled failed");

    let auth_flag = AuthEnabledCache::new();
    auth_flag
        .refresh(&fixture.control_pool)
        .await
        .expect("refresh failed");
    assert!(
        !auth_flag.is_enabled(fixture.tenant_id).await,
        "a freshly refreshed cache must observe the disabled flag"
    );

    let registry = TenantRegistry::new();
    let keystore: Arc<dyn KeyStore> = Arc::new(vault_keystore());
    let tenant = registry
        .get_or_open(
            &fixture.control_pool,
            &fixture.control_url,
            keystore.as_ref(),
            fixture.tenant_id,
            5,
        )
        .await
        .expect("opening tenant context failed");

    let app_state = AppState {
        control_pool: fixture.control_pool.clone(),
        control_database_url: fixture.control_url.clone(),
        keystore,
        registry: Arc::new(registry),
        tenant_pool_max_connections: 5,
        profile: Profile::Dev,
        auth_flag: Arc::new(auth_flag),
        provider_config_cache: Arc::new(ProviderConfigCache::new()),
        sms_base_url: mock_server.uri(),
        buffer_path: fixture.buffer_path.clone(),
        pending_path: fixture.pending_path.clone(),
    };

    let customer_id = Uuid::new_v4();
    let result = send_otp(
        State(app_state.clone()),
        ProducerContext {
            producer_id: Uuid::new_v4(),
            tenant,
        },
        axum::Json(serde_json::from_value(otp_body(customer_id, "+15550301")).unwrap()),
    )
    .await;
    assert!(result.is_err(), "a disabled tenant must not report success");

    assert!(
        mock_server.received_requests().await.unwrap().is_empty(),
        "the provider must receive zero calls while auth is disabled"
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
    let discarded: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM comms_request WHERE final_status = 'discarded'",
    )
    .fetch_one(&tenant_pool)
    .await
    .expect("counting discarded rows failed");
    assert_eq!(discarded, 1, "exactly one discarded row must be written");

    tenant_pool.close().await;
    teardown(&fixture, None).await;
}

#[tokio::test]
async fn provider_failover_tries_the_next_priority() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wpath("/messages"))
        .and(header("Authorization", "Bearer test-key-primary"))
        .respond_with(
            ResponseTemplate::new(500).set_body_string("provider unavailable"),
        )
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(wpath("/messages"))
        .and(header("Authorization", "Bearer test-key-secondary"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "msg-secondary",
            "status": "queued",
        })))
        .mount(&mock_server)
        .await;

    let fixture = setup("en-US", &mock_server.uri()).await;
    let vault = vault_keystore();
    set_provider(&fixture, &vault, 1, "test-key-primary").await;
    set_provider(&fixture, &vault, 2, "test-key-secondary").await;

    let (identity_pem, cert_subject) =
        register_test_producer(&fixture, &vault, "auth-service-failover").await;
    let client = mtls_client(
        &fixture.server_common_name,
        fixture.server.addr,
        &identity_pem,
        &fixture.server_ca_pem,
    );
    let url = format!(
        "https://{}:{}/otp",
        fixture.server_common_name,
        fixture.server.addr.port()
    );

    let customer_id = Uuid::new_v4();
    let response = client
        .post(&url)
        .json(&otp_body(customer_id, "+15550400"))
        .send()
        .await
        .expect("request failed");
    assert_eq!(
        response.status(),
        201,
        "the request must still succeed via the second provider"
    );

    let requests = mock_server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "both providers must have been tried");

    teardown(&fixture, Some(&cert_subject)).await;
}

/// F1 regression: DEK resolution used to run *before* the provider call
/// with a bare `?`, so a Vault/Postgres outage aborted the whole request --
/// contradicting decision 9 ("the send still succeeds") and this ticket's
/// own Outcome. Unlike `postgres_write_failure_buffers_and_drain_recovers_it`
/// above, the DEK cache here is left cold on purpose: `get_or_create_dek`'s
/// own Postgres lookup is what fails, proving DEK resolution itself -- not
/// just the later audit write -- is now best-effort.
#[tokio::test]
async fn dek_resolution_failure_still_sends_and_buffers_pending() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wpath("/messages"))
        .and(header("Authorization", "Bearer test-key-pending"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "msg-pending",
            "status": "queued",
        })))
        .mount(&mock_server)
        .await;

    let fixture = setup("en-US", &mock_server.uri()).await;
    let vault = vault_keystore();
    set_provider(&fixture, &vault, 1, "test-key-pending").await;

    let registry = TenantRegistry::new();
    let keystore: Arc<dyn KeyStore> = Arc::new(vault_keystore());
    let tenant = registry
        .get_or_open(
            &fixture.control_pool,
            &fixture.control_url,
            keystore.as_ref(),
            fixture.tenant_id,
            5,
        )
        .await
        .expect("opening tenant context failed");

    let broken_pool = connect_tenant_pool(
        &fixture.control_pool,
        &fixture.control_url,
        fixture.tenant_id,
        &fixture.database_name,
        2,
    )
    .await
    .expect("opening the pool to break failed")
    .pool;
    broken_pool.close().await;

    let customer_id = Uuid::new_v4();
    let broken_tenant = Arc::new(TenantContext {
        tenant: tenant.tenant.clone(),
        pool: broken_pool,
        config: tenant.config.clone(),
        pepper: tenant.pepper.clone(),
        dek_cache: KeyCache::new(
            std::num::NonZeroUsize::new(4).unwrap(),
            std::time::Duration::from_secs(60),
        ),
        kill_switches: Arc::new(KillSwitchCache::new()),
    });

    let app_state = AppState {
        control_pool: fixture.control_pool.clone(),
        control_database_url: fixture.control_url.clone(),
        keystore: keystore.clone(),
        registry: Arc::new(registry),
        tenant_pool_max_connections: 5,
        profile: Profile::Dev,
        auth_flag: Arc::new(AuthEnabledCache::new()),
        provider_config_cache: Arc::new(ProviderConfigCache::new()),
        sms_base_url: mock_server.uri(),
        buffer_path: fixture.buffer_path.clone(),
        pending_path: fixture.pending_path.clone(),
    };
    app_state
        .auth_flag
        .refresh(&fixture.control_pool)
        .await
        .expect("auth_enabled refresh failed");
    // Primed against the still-working pool, before `tenant.pool` is
    // replaced below -- provider selection and credential reads must
    // survive the same outage that breaks DEK resolution.
    app_state
        .provider_config_cache
        .load(&tenant.pool, fixture.tenant_id)
        .await;

    let (status, response) = send_otp(
        State(app_state.clone()),
        ProducerContext {
            producer_id: Uuid::new_v4(),
            tenant: broken_tenant,
        },
        axum::Json(serde_json::from_value(otp_body(customer_id, "+15550500")).unwrap()),
    )
    .await
    .expect(
        "handler must still return success -- DEK resolution failing must not block the send",
    );

    assert_eq!(status, axum::http::StatusCode::CREATED);
    let comms_request_id = response.0.comms_request_id;

    assert_eq!(
        mock_server.received_requests().await.unwrap().len(),
        1,
        "the provider must still receive exactly one call despite the DEK outage"
    );

    let pending_contents = std::fs::read_to_string(&fixture.pending_path).expect(
        "the pending-crypto buffer file must exist after a DEK resolution failure",
    );
    assert!(
        pending_contents.contains(&comms_request_id.to_string()),
        "the pending record must carry this send's comms_request_id"
    );
    assert!(
        !pending_contents.contains("+15550500"),
        "F5 regression: the destination must never be persisted to the pending-crypto buffer file in plaintext"
    );
    assert!(
        std::fs::read_to_string(&fixture.buffer_path)
            .unwrap_or_default()
            .trim()
            .is_empty(),
        "the write-retry buffer must stay empty -- crypto never completed, so write_audit_record was never attempted"
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
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM comms_request")
        .fetch_one(&tenant_pool)
        .await
        .expect("counting comms_request rows failed");
    assert_eq!(before, 0, "the row must not exist before draining");

    pending::drain(
        &fixture.pending_path,
        &fixture.buffer_path,
        &fixture.control_pool,
        &fixture.control_url,
        keystore.as_ref(),
        &app_state.registry,
        5,
    )
    .await;

    let (final_status, _, _) =
        wait_for_comms_request(&tenant_pool, comms_request_id).await;
    assert_eq!(final_status, "sent", "the drained row must reach sent");

    let remaining_pending =
        std::fs::read_to_string(&fixture.pending_path).unwrap_or_default();
    assert!(
        remaining_pending.trim().is_empty(),
        "a successfully drained pending record must be removed from the pending-crypto buffer file"
    );

    tenant_pool.close().await;
    teardown(&fixture, None).await;
}

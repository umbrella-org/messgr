//! Kill-switch integration suite (DESIGN.md §5.2, T-016), following
//! `tests/dispatcher.rs`/`tests/ingest.rs`'s conventions: real provisioning
//! against the local stack, no mocks except the provider itself.

use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::post;
use axum_server::Handle;
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use chrono::{DateTime, Utc};
use reqwest::{Certificate, Identity};
use sqlx::PgPool;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use messgr::customer_dek::lifecycle::get_or_create_dek;
use messgr::db;
use messgr::dispatcher::drain::drain_released_scope;
use messgr::dispatcher::repo;
use messgr::dispatcher::worker::DispatcherContext;
use messgr::encryption;
use messgr::ingest::AppState;
use messgr::ingest::handler::create_comms;
use messgr::ingest::repo::insert_transactional;
use messgr::key_cache::KeyCache;
use messgr::keystore::{KeyStore, VaultKeyStore};
use messgr::kill_switch::cache::{
    ChannelExclusion, KillSwitchCache, exclusion_for_channel,
};
use messgr::kill_switch::model::{KillSwitch, on_queued, scope};
use messgr::mtls::{self, ClientCertAcceptor};
use messgr::producer::cert_repo;
use messgr::producer::dev_pki;
use messgr::producer::register::register_producer;
use messgr::producer_quota::tracker::QuotaTracker;
use messgr::profile::Profile;
use messgr::sender::Sender;
use messgr::sender::http::HttpSender;
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

fn small_cache() -> KeyCache {
    KeyCache::new(NonZeroUsize::new(8).unwrap(), Duration::from_secs(60))
}

async fn engage_switch(
    pool: &PgPool,
    scope: &str,
    scope_key: Option<&str>,
    on_queued: &str,
) -> KillSwitch {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO kill_switch (id, scope, scope_key, on_queued, engaged_by, engaged_at, reason) \
         VALUES ($1, $2, $3, $4, 'test-actor', now(), 'test')",
    )
    .bind(id)
    .bind(scope)
    .bind(scope_key)
    .bind(on_queued)
    .execute(pool)
    .await
    .expect("engaging test switch failed");

    sqlx::query_as::<_, KillSwitch>(
        "SELECT id, scope, scope_key, on_queued, engaged_by, engaged_at, reason, \
                released_by, released_at FROM kill_switch WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("reading back the engaged switch failed")
}

async fn release_switch(pool: &PgPool, id: Uuid) {
    sqlx::query(
        "UPDATE kill_switch SET released_by = 'test-actor', released_at = now() WHERE id = $1",
    )
    .bind(id)
    .execute(pool)
    .await
    .expect("releasing test switch failed");
}

// --- dispatcher-repo-level fixtures (tests/dispatcher.rs's conventions) ---

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

    let slug = unique_name("test_kill_switch");
    let database_name = unique_name("test_db_kill_switch");

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

#[allow(clippy::too_many_arguments)]
async fn write_outbox_row(
    tenant: &TestTenant,
    vault: &VaultKeyStore,
    cache: &KeyCache,
    channel: &str,
    producer_id: Uuid,
    campaign_id: Option<&str>,
    destination: &str,
) -> Uuid {
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
    let payload_ciphertext = encryption::encrypt(&dek, aad, b"hello there")
        .expect("encrypting payload failed");

    insert_transactional(
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
        campaign_id,
        b"unused-hmac",
        &destination_ciphertext,
        &payload_ciphertext,
        producer_id,
        Uuid::new_v4(),
        None,
        None,
    )
    .await
    .expect("insert_transactional failed");

    comms_request_id
}

async fn outbox_leased_until(
    pool: &PgPool,
    comms_request_id: Uuid,
) -> Option<DateTime<Utc>> {
    sqlx::query_scalar("SELECT leased_until FROM outbox WHERE comms_request_id = $1")
        .bind(comms_request_id)
        .fetch_one(pool)
        .await
        .expect("outbox row must still exist")
}

async fn final_status(pool: &PgPool, comms_request_id: Uuid) -> Option<String> {
    sqlx::query_scalar("SELECT final_status FROM comms_request WHERE id = $1")
        .bind(comms_request_id)
        .fetch_one(pool)
        .await
        .expect("comms_request row must exist")
}

fn cache_exclusion(active: &[KillSwitch], channel: &str) -> ChannelExclusion {
    exclusion_for_channel(active.iter(), channel)
}

#[tokio::test]
async fn engaged_producer_switch_excludes_only_that_producers_rows() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let blocked_producer = Uuid::new_v4();
    let other_producer = Uuid::new_v4();
    let blocked_id = write_outbox_row(
        &tenant,
        &vault,
        &cache,
        "sms",
        blocked_producer,
        None,
        "+15550100",
    )
    .await;
    let other_id = write_outbox_row(
        &tenant,
        &vault,
        &cache,
        "sms",
        other_producer,
        None,
        "+15550101",
    )
    .await;

    let switch = engage_switch(
        &tenant.tenant_pool,
        scope::PRODUCER,
        Some(&blocked_producer.to_string()),
        on_queued::HOLD,
    )
    .await;

    let exclusion = cache_exclusion(&[switch], "sms");
    let claimed = repo::claim(
        &tenant.tenant_pool,
        "sms",
        10,
        Utc::now() + chrono::Duration::minutes(2),
        &exclusion,
    )
    .await
    .expect("claim failed");

    let claimed_ids: Vec<Uuid> =
        claimed.iter().map(|row| row.comms_request_id).collect();
    assert!(
        !claimed_ids.contains(&blocked_id),
        "a row from the blocked producer must not be claimable"
    );
    assert!(
        claimed_ids.contains(&other_id),
        "a row from an unrelated producer must still be claimable"
    );
    assert_eq!(
        outbox_leased_until(&tenant.tenant_pool, blocked_id).await,
        None,
        "the held row must never have been leased"
    );

    tenant.cleanup().await;
}

#[tokio::test]
async fn global_switch_excludes_every_channel() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let producer_id = Uuid::new_v4();
    write_outbox_row(
        &tenant,
        &vault,
        &cache,
        "sms",
        producer_id,
        None,
        "+15550100",
    )
    .await;
    write_outbox_row(
        &tenant,
        &vault,
        &cache,
        "email",
        producer_id,
        None,
        "jordan@example.com",
    )
    .await;

    let switch =
        engage_switch(&tenant.tenant_pool, scope::GLOBAL, None, on_queued::HOLD).await;

    for channel in ["sms", "email"] {
        let exclusion = cache_exclusion(std::slice::from_ref(&switch), channel);
        assert!(
            exclusion.blocked_entirely,
            "a global switch must block every channel, including {channel}"
        );
        let claimed = repo::claim(
            &tenant.tenant_pool,
            channel,
            10,
            Utc::now() + chrono::Duration::minutes(2),
            &exclusion,
        )
        .await
        .expect("claim failed");
        assert!(
            claimed.is_empty(),
            "{channel} must claim nothing under a global switch"
        );
    }

    tenant.cleanup().await;
}

#[tokio::test]
async fn release_ramp_admits_at_most_release_rate_rows_per_batch() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let producer_id = Uuid::new_v4();
    for i in 0..5 {
        write_outbox_row(
            &tenant,
            &vault,
            &cache,
            "sms",
            producer_id,
            None,
            &format!("+1555020{i}"),
        )
        .await;
    }

    let switch = engage_switch(
        &tenant.tenant_pool,
        scope::PRODUCER,
        Some(&producer_id.to_string()),
        on_queued::HOLD,
    )
    .await;

    // Not yet released: claim_for_scope still finds the whole backlog (it's
    // the drain task's own claim, not gated by the exclusion cache), but the
    // *rate* -- not the scope -- is what bounds one batch's size.
    let batch = repo::claim_for_scope(
        &tenant.tenant_pool,
        Some("sms"),
        &switch,
        2,
        Utc::now() + chrono::Duration::minutes(2),
        &ChannelExclusion::default(),
    )
    .await
    .expect("claim_for_scope failed");
    assert_eq!(
        batch.len(),
        2,
        "one drain batch must never exceed the configured release rate"
    );

    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox WHERE leased_until IS NULL")
            .fetch_one(&tenant.tenant_pool)
            .await
            .expect("counting unleased rows failed");
    assert_eq!(
        remaining, 3,
        "rows beyond one batch must remain unleased until the next tick"
    );

    let _ = release_switch(&tenant.tenant_pool, switch.id).await;
    tenant.cleanup().await;
}

#[tokio::test]
async fn drain_sends_every_row_and_marks_an_already_expired_one_expired_instead() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let cache = small_cache();

    let producer_id = Uuid::new_v4();
    let fresh_id = write_outbox_row(
        &tenant,
        &vault,
        &cache,
        "sms",
        producer_id,
        None,
        "+15550300",
    )
    .await;
    let expired_id = write_outbox_row(
        &tenant,
        &vault,
        &cache,
        "sms",
        producer_id,
        None,
        "+15550301",
    )
    .await;
    sqlx::query("UPDATE outbox SET expires_at = now() - interval '1 hour' WHERE comms_request_id = $1")
        .bind(expired_id)
        .execute(&tenant.tenant_pool)
        .await
        .expect("backdating expires_at failed");

    let switch = engage_switch(
        &tenant.tenant_pool,
        scope::PRODUCER,
        Some(&producer_id.to_string()),
        on_queued::HOLD,
    )
    .await;
    release_switch(&tenant.tenant_pool, switch.id).await;
    let mut released_switch = switch;
    released_switch.released_at = Some(Utc::now());

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "msg-drain-1",
            "status": "queued",
        })))
        .mount(&mock_server)
        .await;
    let sender: Arc<dyn Sender> =
        Arc::new(HttpSender::new(mock_server.uri(), "test-key".to_string()));

    let ctx = Arc::new(DispatcherContext {
        pool: tenant.tenant_pool.clone(),
        keystore: Arc::new(vault_keystore()),
        cache: Arc::new(cache),
        mount: tenant.mount.clone(),
        sender,
        verification_mode: "observe".to_string(),
        default_timezone: "UTC".to_string(),
        quiet_hours_policy: None,
        kill_switches: Arc::new(KillSwitchCache::new()),
        draining: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(QuotaTracker::new("UTC")),
    });

    drain_released_scope(ctx, "sms".to_string(), released_switch, 10).await;

    assert_eq!(
        final_status(&tenant.tenant_pool, fresh_id).await.as_deref(),
        Some("sent")
    );
    assert_eq!(
        final_status(&tenant.tenant_pool, expired_id)
            .await
            .as_deref(),
        Some("expired"),
        "a row whose expires_at had already passed by drain time must not be sent"
    );

    tenant.cleanup().await;
}

#[tokio::test]
async fn release_immediately_stops_blocking_new_ingest_even_before_drain_finishes() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;

    let producer_id = Uuid::new_v4();
    let switch = engage_switch(
        &tenant.tenant_pool,
        scope::PRODUCER,
        Some(&producer_id.to_string()),
        on_queued::HOLD,
    )
    .await;

    let ingest_cache = KillSwitchCache::new();
    ingest_cache
        .refresh(&tenant.tenant_pool)
        .await
        .expect("initial refresh failed");
    assert_eq!(
        ingest_cache.blocking_scope("sms", producer_id, None).await,
        Some(scope::PRODUCER.to_string()),
        "an engaged switch must block matching new ingests"
    );

    release_switch(&tenant.tenant_pool, switch.id).await;
    ingest_cache
        .refresh(&tenant.tenant_pool)
        .await
        .expect("post-release refresh failed");
    assert_eq!(
        ingest_cache.blocking_scope("sms", producer_id, None).await,
        None,
        "ingest must accept new sends the moment a switch releases, independent of any \
         dispatcher-side drain ramp still in progress"
    );

    tenant.cleanup().await;
}

// --- HTTP-level: ingest rejects a matching request under an engaged switch ---

struct TestServer {
    addr: SocketAddr,
    handle: Handle<SocketAddr>,
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

async fn setup(locale: &str) -> Fixture {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("connecting to control database failed");
    let vault = vault_keystore();

    let tenant_slug = unique_name("test_tenant_kill_switch");
    let database_name = unique_name("test_db_kill_switch_http");

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

    let server_common_name = unique_hostname("messgr-kill-switch.test");
    let server_cert =
        dev_pki::issue_server_cert(vault.client(), Profile::Dev, &server_common_name)
            .await
            .expect("issuing server certificate failed");

    let cert_dir = std::env::temp_dir().join(unique_name("messgr-kill-switch-test"));
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

async fn register_test_producer(
    fixture: &Fixture,
    vault: &VaultKeyStore,
    name: &str,
) -> (String, String, Uuid) {
    let common_name = unique_name(name);
    let cert_subject = format!("CN={common_name}");

    let outcome = register_producer(
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
    (identity_pem, cert_subject, outcome.producer_id)
}

fn sample_body(customer_id: Uuid, destination: &str) -> serde_json::Value {
    serde_json::json!({
        "customer_id": customer_id,
        "destination": destination,
        "channel": "sms",
        "class": "transactional",
        "template_id": "balance-alert",
        "template_version": 1,
        "variables": { "name": "Jordan", "balance": "100.00" }
    })
}

async fn teardown(fixture: &Fixture, cert_subjects: &[&str]) {
    fixture.server.shutdown();
    for cert_subject in cert_subjects {
        let _ =
            cert_repo::delete_producer_cert(&fixture.control_pool, cert_subject).await;
    }
    let _ = std::fs::remove_dir_all(&fixture.cert_dir);
    drop_test_tenant(
        &fixture.control_pool,
        &fixture.database_name,
        &fixture.tenant_slug,
    )
    .await;
}

#[tokio::test]
async fn engaged_kill_switch_rejects_the_matching_producer_but_not_another() {
    let fixture = setup("en-US").await;
    let vault = vault_keystore();
    let (blocked_identity, blocked_subject, blocked_producer_id) =
        register_test_producer(&fixture, &vault, "blocked-caller").await;
    let (other_identity, other_subject, _other_producer_id) =
        register_test_producer(&fixture, &vault, "other-caller").await;

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
    engage_switch(
        &tenant_pool,
        scope::PRODUCER,
        Some(&blocked_producer_id.to_string()),
        on_queued::HOLD,
    )
    .await;

    let url = format!(
        "https://{}:{}/comms",
        fixture.server_common_name,
        fixture.server.addr.port()
    );

    let blocked_client = mtls_client(
        &fixture.server_common_name,
        fixture.server.addr,
        &blocked_identity,
        &fixture.server_ca_pem,
    );
    // The ingest-side cache is poll-refreshed (every few seconds), not
    // synchronous with the psql `INSERT` above -- wait for it rather than
    // asserting immediately.
    let mut last_status = None;
    for i in 0..20 {
        let response = blocked_client
            .post(&url)
            .header("Idempotency-Key", unique_name("idem"))
            .json(&sample_body(Uuid::new_v4(), &format!("+155502{i:02}")))
            .send()
            .await
            .expect("request failed");
        last_status = Some(response.status());
        if response.status() == 503 {
            let body: serde_json::Value =
                response.json().await.expect("parsing response failed");
            assert!(
                body["error"].as_str().unwrap().contains("kill switch"),
                "unexpected error body: {body:?}"
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert_eq!(
        last_status,
        Some(reqwest::StatusCode::SERVICE_UNAVAILABLE),
        "the blocked producer must eventually see 503 once its ingest process's cache refreshes"
    );

    let other_client = mtls_client(
        &fixture.server_common_name,
        fixture.server.addr,
        &other_identity,
        &fixture.server_ca_pem,
    );
    let response = other_client
        .post(&url)
        .header("Idempotency-Key", unique_name("idem"))
        .json(&sample_body(Uuid::new_v4(), "+15559999"))
        .send()
        .await
        .expect("request failed");
    assert_eq!(
        response.status(),
        201,
        "a producer not named by the switch must be unaffected"
    );

    tenant_pool.close().await;
    teardown(&fixture, &[&blocked_subject, &other_subject]).await;
}

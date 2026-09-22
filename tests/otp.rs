//! `messgr-otp` acceptance suite (T-056), mirroring `tests/sms_sender.rs`'s
//! conventions: real provisioning against the local stack, no mocks except
//! the outbound SMS provider (`wiremock`) and a call-counting `KeyStore`
//! wrapper used only to prove `OtpProviderCache` never re-reads Vault.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::extract::State;
use uuid::Uuid;
use wiremock::MockServer;
use zeroize::Zeroizing;

use messgr::db;
use messgr::keystore::{Dek, KeyStore, KeyStoreError, VaultKeyStore};
use messgr::otp::AppState;
use messgr::otp::auth_flag::AuthEnabledCache;
use messgr::otp::identity::ProducerContext;
use messgr::otp::provider::{OtpProviderCache, refresh_once};
use messgr::profile::Profile;
use messgr::provider_config::configure::set_provider_config;
use messgr::provider_config::model::ProviderConfigInput;
use messgr::tenant::pool::connect_tenant_pool;
use messgr::tenant::provision::provision_tenant;
use messgr::tenant::registry::TenantRegistry;
use messgr::tenant_config::configure::set_tenant_config;
use messgr::tenant_config::model::{TenantConfigInput, verification_mode};
use sqlx::PgPool;

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

struct Fixture {
    control_pool: PgPool,
    control_url: String,
    tenant_id: Uuid,
    tenant_slug: String,
    database_name: String,
    buffer_path: PathBuf,
    pending_path: PathBuf,
}

async fn teardown(fixture: &Fixture) {
    let _ = std::fs::remove_file(&fixture.buffer_path);
    let _ = std::fs::remove_file(&fixture.pending_path);
    drop_test_tenant(
        &fixture.control_pool,
        &fixture.database_name,
        &fixture.tenant_slug,
    )
    .await;
}

/// Provisions a tenant and sets its config -- no mTLS server here (unlike
/// `tests/sms_sender.rs`'s `Fixture`): both tests below drive
/// `OtpProviderCache`/`send_otp` directly, the same "exercise the handler
/// function directly" style `tests/sms_sender.rs::auth_disabled_tenant_never_calls_provider`
/// already uses.
async fn setup(locale: &str) -> Fixture {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("connecting to control database failed");
    let vault = vault_keystore();

    let tenant_slug = unique_name("test_tenant_otp");
    let database_name = unique_name("test_db_otp");

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

    let buffer_path = std::env::temp_dir()
        .join(unique_name("otp-buffer"))
        .with_extension("jsonl");
    let pending_path = std::env::temp_dir()
        .join(unique_name("otp-pending"))
        .with_extension("jsonl");

    Fixture {
        control_pool,
        control_url,
        tenant_id: provision_outcome.tenant_id,
        tenant_slug,
        database_name,
        buffer_path,
        pending_path,
    }
}

/// Sets one `provider_config` row and seeds its Vault KV credential.
async fn set_provider(
    fixture: &Fixture,
    vault: &VaultKeyStore,
    priority: i16,
    api_key: &str,
) {
    let kv_path = format!("{}/otp-priority-{priority}", fixture.tenant_slug);
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

/// Wraps a real `VaultKeyStore` and counts `read_provider_credential`
/// calls -- proves `OtpProviderCache::get_or_fetch` resolves Vault exactly
/// once per tenant and serves every later call from the cache.
struct CountingKeyStore {
    inner: VaultKeyStore,
    read_provider_credential_calls: AtomicUsize,
}

#[async_trait]
impl KeyStore for CountingKeyStore {
    async fn create_dek(&self, mount: &str) -> Result<Dek, KeyStoreError> {
        self.inner.create_dek(mount).await
    }

    async fn unwrap_dek(
        &self,
        mount: &str,
        wrapped: &str,
    ) -> Result<Zeroizing<Vec<u8>>, KeyStoreError> {
        self.inner.unwrap_dek(mount, wrapped).await
    }

    async fn read_provider_credential(
        &self,
        mount: &str,
        path: &str,
    ) -> Result<String, KeyStoreError> {
        self.read_provider_credential_calls
            .fetch_add(1, Ordering::SeqCst);
        self.inner.read_provider_credential(mount, path).await
    }
}

#[tokio::test]
async fn provider_cache_resolves_once_and_serves_the_cache_on_a_second_call() {
    let fixture = setup("en-US").await;
    let vault = vault_keystore();
    set_provider(&fixture, &vault, 1, "test-key-otp-1").await;

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

    let keystore = CountingKeyStore {
        inner: vault_keystore(),
        read_provider_credential_calls: AtomicUsize::new(0),
    };
    let cache = OtpProviderCache::new();

    let first = cache
        .get_or_fetch(&tenant_pool, &keystore, fixture.tenant_id)
        .await;
    assert_eq!(first.len(), 1, "one provider_config row must resolve");
    assert_eq!(first[0].api_key, "test-key-otp-1");

    let second = cache
        .get_or_fetch(&tenant_pool, &keystore, fixture.tenant_id)
        .await;
    assert_eq!(second.len(), 1);

    assert_eq!(
        keystore
            .read_provider_credential_calls
            .load(Ordering::SeqCst),
        1,
        "a second get_or_fetch for the same tenant must not re-read Vault"
    );

    tenant_pool.close().await;
    teardown(&fixture).await;
}

#[tokio::test]
async fn refresh_once_updates_a_cached_entry_and_leaves_it_untouched_on_failure() {
    let fixture = setup("en-US").await;
    let vault = vault_keystore();
    set_provider(&fixture, &vault, 1, "test-key-otp-refresh-1").await;

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

    let keystore: Arc<dyn KeyStore> = Arc::new(vault_keystore());
    let cache = OtpProviderCache::new();
    let registry = TenantRegistry::new();

    // Seed the cache the way a real first request would.
    let before = cache
        .get_or_fetch(&tenant_pool, keystore.as_ref(), fixture.tenant_id)
        .await;
    assert_eq!(before.len(), 1);

    // A second provider is added after the cache was populated -- a real
    // tick must pick it up.
    set_provider(&fixture, &vault, 2, "test-key-otp-refresh-2").await;

    refresh_once(
        &cache,
        &fixture.control_pool,
        &fixture.control_url,
        keystore.as_ref(),
        &registry,
        5,
    )
    .await;

    let after = cache
        .get_or_fetch(&tenant_pool, keystore.as_ref(), fixture.tenant_id)
        .await;
    assert_eq!(
        after.len(),
        2,
        "a successful refresh tick must pick up the newly added provider"
    );

    // A tenant id with no `tenant` row at all -- `TenantRegistry::get_or_open`
    // fails for it, simulating a reload failure without needing to break a
    // real pool. Seed a cache entry for it directly through `get_or_fetch`
    // (which only needs a working pool, not a real tenant identity) using
    // the fixture's own tenant pool, then confirm `refresh_once` leaves that
    // entry untouched when the tenant lookup fails.
    let unknown_tenant_id = Uuid::new_v4();
    let before_unknown = cache
        .get_or_fetch(&tenant_pool, keystore.as_ref(), unknown_tenant_id)
        .await;
    assert_eq!(before_unknown.len(), 2);

    refresh_once(
        &cache,
        &fixture.control_pool,
        &fixture.control_url,
        keystore.as_ref(),
        &registry,
        5,
    )
    .await;

    let after_unknown = cache
        .get_or_fetch(&tenant_pool, keystore.as_ref(), unknown_tenant_id)
        .await;
    assert_eq!(
        after_unknown.len(),
        2,
        "a reload failure (unknown tenant) must leave the existing cached entry untouched"
    );

    tenant_pool.close().await;
    teardown(&fixture).await;
}

/// Exercises the handler function directly, same reasoning
/// `tests/sms_sender.rs::auth_disabled_tenant_never_calls_provider` gives:
/// `AuthEnabledCache` refreshes on a poll, so this proves the cache read and
/// the handler's branch on it, not the poll timer itself.
#[tokio::test]
async fn auth_disabled_tenant_discards_without_calling_the_provider() {
    let mock_server = MockServer::start().await;
    // No Mock mounted at all -- any call the handler made to this server
    // would fail to match and the test would fail on the assertion below,
    // not silently pass.

    let fixture = setup("en-US").await;

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
        auth_flag: Arc::new(auth_flag),
        provider_cache: Arc::new(OtpProviderCache::new()),
        otp_base_url: mock_server.uri(),
        buffer_path: fixture.buffer_path.clone(),
        pending_path: fixture.pending_path.clone(),
    };

    let customer_id = Uuid::new_v4();
    let result = messgr::otp::handler::send_otp(
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
    teardown(&fixture).await;
}

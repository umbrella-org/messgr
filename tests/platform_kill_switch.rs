//! Platform-tier kill switches (T-058, DESIGN.md §5.2 "Two tiers in
//! cloud"), against the local stack: constraints and audit, the cache merge
//! that makes a platform switch block dispatch and ingest, the per-tenant
//! `NOTIFY` fan-out, and the release drain respecting still-engaged
//! switches.
//!
//! Every test runs through `isolated`, which serializes this whole file and
//! releases any switch this suite engaged — even when the test body panics.
//! A region-wide switch left engaged would otherwise block every tenant in
//! every later test run against the same control database.

use std::collections::HashMap;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::sync::Mutex;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use messgr::customer_dek::lifecycle::get_or_create_dek;
use messgr::db;
use messgr::dispatcher::drain::drain_released_scope;
use messgr::dispatcher::worker::DispatcherContext;
use messgr::encryption;
use messgr::ingest::repo::insert_transactional;
use messgr::key_cache::KeyCache;
use messgr::keystore::VaultKeyStore;
use messgr::kill_switch::cache::{
    KillSwitchCache, PLATFORM_SCOPE, exclusion_for_channel,
};
use messgr::kill_switch::model::{KillSwitch, on_queued, scope as tenant_scope};
use messgr::platform_kill_switch::configure::{self, ConfigureError};
use messgr::platform_kill_switch::model::{PlatformKillSwitch, scope};
use messgr::producer_quota::tracker::QuotaTracker;
use messgr::profile::Profile;
use messgr::sender::Sender;
use messgr::sender::http::HttpSender;
use messgr::tenant::pool::connect_tenant_pool;
use messgr::tenant::provision::provision_tenant;
use messgr::tenant::registry::TenantRegistry;
use messgr::tenant_config::configure::set_tenant_config;
use messgr::tenant_config::model::{TenantConfigInput, verification_mode};

/// Every switch this suite engages carries this actor, so `isolated` can
/// find and release exactly its own leftovers and nobody else's.
const TEST_ACTOR: &str = "test-platform-kill-switch";
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(5);

static SERIAL: Mutex<()> = Mutex::const_new(());

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

async fn control_pool() -> PgPool {
    db::connect(&control_database_url(), 5)
        .await
        .expect("failed to connect to control database")
}

async fn release_suite_leftovers(control_pool: &PgPool) {
    sqlx::query(
        "UPDATE platform_kill_switch SET released_by = $1, released_at = now() \
         WHERE engaged_by = $1 AND released_at IS NULL",
    )
    .bind(TEST_ACTOR)
    .execute(control_pool)
    .await
    .expect("releasing this suite's leftover platform switches failed");
}

/// Runs `body` serialized against every other test in this file, releasing
/// this suite's switches before and after — the "after" even on panic,
/// which is re-raised once the release has run.
async fn isolated<F, Fut>(body: F)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let _guard = SERIAL.lock().await;
    let control_pool = control_pool().await;
    release_suite_leftovers(&control_pool).await;
    let outcome = tokio::spawn(body()).await;
    release_suite_leftovers(&control_pool).await;
    if let Err(err) = outcome {
        std::panic::resume_unwind(err.into_panic());
    }
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
    let control_pool = control_pool().await;
    let slug = unique_name("test_platform_ks");
    let database_name = unique_name("test_db_platform_ks");

    let outcome = provision_tenant(
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
        outcome.tenant_id,
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
        tenant_id: outcome.tenant_id,
        mount: format!("transit/{slug}"),
        slug,
        database_name,
    }
}

impl TestTenant {
    async fn cleanup(self) {
        self.tenant_pool.close().await;
        let pool = &self.control_pool;
        let terminate = format!(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{}'",
            self.database_name
        );
        let _ = sqlx::query(&terminate).execute(pool).await;
        let _ = sqlx::query(&format!(
            "DROP DATABASE IF EXISTS \"{}\"",
            self.database_name
        ))
        .execute(pool)
        .await;
        // platform_kill_switch references tenant (0006's FK) -- its rows go
        // before the tenant row, or the tenant delete fails.
        for table in [
            "platform_kill_switch",
            "tenant_schema_version",
            "platform_audit",
        ] {
            let _ = sqlx::query(&format!("DELETE FROM {table} WHERE tenant_id = $1"))
                .bind(self.tenant_id)
                .execute(pool)
                .await;
        }
        let _ = sqlx::query("DELETE FROM tenant WHERE id = $1")
            .bind(self.tenant_id)
            .execute(pool)
            .await;
    }

    fn platform_source(&self) -> Option<(&PgPool, Uuid)> {
        Some((&self.control_pool, self.tenant_id))
    }
}

async fn engage(
    tenant: &TestTenant,
    scope_value: &str,
    tenant_id: Option<Uuid>,
) -> Result<Uuid, ConfigureError> {
    configure::engage(
        &tenant.control_pool,
        &tenant.control_url,
        scope_value,
        tenant_id,
        "abuse investigation",
        TEST_ACTOR,
    )
    .await
    .map(|(outcome, _)| outcome.id)
}

async fn release(tenant: &TestTenant, id: Uuid) -> &'static str {
    configure::release(&tenant.control_pool, &tenant.control_url, id, TEST_ACTOR)
        .await
        .expect("release failed")
        .0
        .outcome
}

async fn engage_audit_outcomes(pool: &PgPool, tenant_id: Uuid) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT detail->>'outcome' FROM platform_audit \
         WHERE tenant_id = $1 AND action = 'platform_kill_switch.engage' ORDER BY at",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await
    .expect("reading platform_audit failed")
}

async fn kill_switch_listener(tenant: &TestTenant) -> PgListener {
    let mut listener = PgListener::connect_with(&tenant.tenant_pool)
        .await
        .expect("opening LISTEN connection failed");
    listener
        .listen("kill_switch")
        .await
        .expect("LISTEN kill_switch failed");
    listener
}

async fn notified(listener: &mut PgListener) -> bool {
    tokio::time::timeout(NOTIFY_TIMEOUT, listener.recv())
        .await
        .is_ok_and(|received| received.is_ok())
}

fn sample_row(scope_value: &str, tenant_id: Option<Uuid>) -> PlatformKillSwitch {
    PlatformKillSwitch {
        id: Uuid::new_v4(),
        scope: scope_value.to_string(),
        tenant_id,
        engaged_by: TEST_ACTOR.to_string(),
        engaged_at: Utc::now(),
        reason: "test".to_string(),
        released_by: None,
        released_at: None,
    }
}

#[test]
fn platform_and_own_tenant_rows_apply_another_tenants_does_not() {
    let this_tenant = Uuid::new_v4();
    let other_tenant = Uuid::new_v4();

    assert!(sample_row(scope::PLATFORM, None).applies_to(this_tenant));
    assert!(sample_row(scope::TENANT, Some(this_tenant)).applies_to(this_tenant));
    assert!(!sample_row(scope::TENANT, Some(other_tenant)).applies_to(this_tenant));

    let synthetic = sample_row(scope::TENANT, Some(this_tenant)).as_kill_switch();
    assert_eq!(synthetic.scope, tenant_scope::GLOBAL);
    assert_eq!(
        synthetic.on_queued,
        on_queued::HOLD,
        "a platform switch must never discard the tenant's queued messages"
    );
}

#[tokio::test]
async fn a_second_live_switch_for_the_same_tenant_is_already_engaged_and_both_are_audited()
 {
    isolated(|| async {
        let vault = vault_keystore();
        let tenant = provision_test_tenant(&vault).await;

        let id = engage(&tenant, scope::TENANT, Some(tenant.tenant_id))
            .await
            .expect("first engage failed");
        let second = engage(&tenant, scope::TENANT, Some(tenant.tenant_id)).await;
        assert!(matches!(second, Err(ConfigureError::AlreadyEngaged)));
        assert_eq!(
            engage_audit_outcomes(&tenant.control_pool, tenant.tenant_id).await,
            vec!["engaged", "already_engaged"]
        );

        // Once released, the tenant can be switched again.
        assert_eq!(release(&tenant, id).await, "released");
        assert_eq!(release(&tenant, id).await, "idempotent");
        engage(&tenant, scope::TENANT, Some(tenant.tenant_id))
            .await
            .expect("re-engaging after release failed");

        tenant.cleanup().await;
    })
    .await;
}

#[tokio::test]
async fn scope_and_tenant_must_agree() {
    isolated(|| async {
        let vault = vault_keystore();
        let tenant = provision_test_tenant(&vault).await;

        assert!(matches!(
            engage(&tenant, scope::PLATFORM, Some(tenant.tenant_id)).await,
            Err(ConfigureError::Rejected(_))
        ));
        assert!(matches!(
            engage(&tenant, scope::TENANT, None).await,
            Err(ConfigureError::Rejected(_))
        ));
        assert!(matches!(
            engage(&tenant, "region", None).await,
            Err(ConfigureError::Rejected(_))
        ));
        assert_eq!(
            engage_audit_outcomes(&tenant.control_pool, tenant.tenant_id).await,
            vec!["rejected"],
            "the rejected tenant-naming engage is audited against its tenant"
        );

        // The table itself refuses what configure refuses, for a row
        // written from outside it (a psql session).
        let raw = sqlx::query(
            "INSERT INTO platform_kill_switch (id, scope, tenant_id, engaged_by, engaged_at, reason) \
             VALUES ($1, 'platform', $2, $3, now(), 'raw')",
        )
        .bind(Uuid::new_v4())
        .bind(tenant.tenant_id)
        .bind(TEST_ACTOR)
        .execute(&tenant.control_pool)
        .await;
        assert!(raw.is_err(), "a platform-scope row naming a tenant must violate the CHECK");

        tenant.cleanup().await;
    })
    .await;
}

#[tokio::test]
async fn a_tenant_switch_blocks_only_its_tenant_and_its_release_is_reported_for_the_drain()
 {
    isolated(|| async {
        let vault = vault_keystore();
        let tenant = provision_test_tenant(&vault).await;
        let bystander = provision_test_tenant(&vault).await;

        let id = engage(&tenant, scope::TENANT, Some(tenant.tenant_id))
            .await
            .expect("engage failed");

        // The tenant's own `kill_switch` table is empty: a tenant-only
        // refresh sees nothing, so the block below comes from the platform
        // merge alone (deleting it turns this test red).
        let tenant_only = KillSwitchCache::new();
        tenant_only.refresh(&tenant.tenant_pool).await.expect("refresh failed");
        assert!(tenant_only.active_snapshot().await.is_empty());

        let cache = KillSwitchCache::new();
        let delta = cache
            .refresh_with_platform(&tenant.tenant_pool, tenant.platform_source())
            .await
            .expect("refresh failed");
        assert_eq!(delta.newly_engaged.len(), 1);
        for channel in ["sms", "email", "whatsapp"] {
            assert!(
                exclusion_for_channel(cache.active_snapshot().await.iter(), channel)
                    .blocked_entirely,
                "a platform switch must block {channel} entirely"
            );
        }
        assert_eq!(
            cache.blocking_scope("sms", Uuid::new_v4(), None).await.as_deref(),
            Some(PLATFORM_SCOPE)
        );

        let bystander_cache = KillSwitchCache::new();
        bystander_cache
            .refresh_with_platform(&bystander.tenant_pool, bystander.platform_source())
            .await
            .expect("refresh failed");
        assert!(
            bystander_cache.active_snapshot().await.is_empty(),
            "another tenant's switch must not block this one"
        );

        release(&tenant, id).await;
        let delta = cache
            .refresh_with_platform(&tenant.tenant_pool, tenant.platform_source())
            .await
            .expect("refresh failed");
        assert_eq!(delta.released.len(), 1);
        let released = &delta.released[0];
        assert_eq!(released.id, id);
        assert_eq!(
            (released.scope.as_str(), released.on_queued.as_str()),
            (tenant_scope::GLOBAL, on_queued::HOLD),
            "the release must look like a held global switch, which the dispatcher ramps"
        );
        assert_eq!(cache.blocking_scope("sms", Uuid::new_v4(), None).await, None);

        tenant.cleanup().await;
        bystander.cleanup().await;
    })
    .await;
}

/// A control-database outage must neither release a platform switch nor
/// stop the tenant's own switches taking effect.
#[tokio::test]
async fn a_failed_platform_read_keeps_the_platform_switch_and_still_refreshes_tenant_switches()
 {
    isolated(|| async {
        let vault = vault_keystore();
        let tenant = provision_test_tenant(&vault).await;
        let platform_id = engage(&tenant, scope::TENANT, Some(tenant.tenant_id))
            .await
            .expect("engage failed");

        let cache = KillSwitchCache::new();
        cache
            .refresh_with_platform(&tenant.tenant_pool, tenant.platform_source())
            .await
            .expect("refresh failed");

        let tenant_switch = engage_tenant_kill_switch(
            &tenant.tenant_pool,
            tenant_scope::CHANNEL,
            Some("sms"),
        )
        .await;
        let unreachable_control = control_pool().await;
        unreachable_control.close().await;

        let delta = cache
            .refresh_with_platform(
                &tenant.tenant_pool,
                Some((&unreachable_control, tenant.tenant_id)),
            )
            .await
            .expect("a platform read failure must not fail the refresh");
        assert!(
            delta.released.is_empty(),
            "the platform switch must not look released"
        );
        assert_eq!(
            delta.newly_engaged.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![tenant_switch.id],
            "the tenant's own new switch must still take effect"
        );
        assert_eq!(
            cache
                .blocking_scope("email", Uuid::new_v4(), None)
                .await
                .as_deref(),
            Some(PLATFORM_SCOPE)
        );
        assert!(
            cache
                .active_snapshot()
                .await
                .iter()
                .any(|s| s.id == platform_id)
        );

        release_tenant_kill_switch(&tenant.tenant_pool, tenant_switch).await;
        tenant.cleanup().await;
    })
    .await;
}

#[tokio::test]
async fn engage_and_release_notify_only_the_affected_tenant_database() {
    isolated(|| async {
        let vault = vault_keystore();
        let tenant = provision_test_tenant(&vault).await;
        let bystander = provision_test_tenant(&vault).await;
        let mut listener = kill_switch_listener(&tenant).await;
        let mut bystander_listener = kill_switch_listener(&bystander).await;

        let (outcome, report) = configure::engage(
            &tenant.control_pool,
            &tenant.control_url,
            scope::TENANT,
            Some(tenant.tenant_id),
            "non-payment",
            TEST_ACTOR,
        )
        .await
        .expect("engage failed");
        assert_eq!(report.notified, 1);
        assert!(
            notified(&mut listener).await,
            "engage must NOTIFY the tenant's database"
        );

        let (_, report) = configure::release(
            &tenant.control_pool,
            &tenant.control_url,
            outcome.id,
            TEST_ACTOR,
        )
        .await
        .expect("release failed");
        assert_eq!(report.notified, 1);
        assert!(
            notified(&mut listener).await,
            "release must NOTIFY the tenant's database"
        );

        assert!(
            tokio::time::timeout(Duration::from_millis(500), bystander_listener.recv())
                .await
                .is_err(),
            "a tenant-scope switch must not NOTIFY any other tenant"
        );

        // Each listener holds a pool connection; `cleanup`'s `pool.close()`
        // waits for it, so the listeners must go first.
        drop(listener);
        drop(bystander_listener);
        tenant.cleanup().await;
        bystander.cleanup().await;
    })
    .await;
}

#[tokio::test]
async fn a_region_wide_switch_blocks_and_notifies_every_tenant() {
    isolated(|| async {
        let vault = vault_keystore();
        let first = provision_test_tenant(&vault).await;
        let second = provision_test_tenant(&vault).await;
        let mut first_listener = kill_switch_listener(&first).await;
        let mut second_listener = kill_switch_listener(&second).await;

        let id = engage(&first, scope::PLATFORM, None)
            .await
            .expect("engaging region-wide switch failed");

        for (tenant, listener) in [
            (&first, &mut first_listener),
            (&second, &mut second_listener),
        ] {
            assert!(
                notified(listener).await,
                "a region-wide switch must NOTIFY tenant {}",
                tenant.slug
            );
            let cache = KillSwitchCache::new();
            cache
                .refresh_with_platform(&tenant.tenant_pool, tenant.platform_source())
                .await
                .expect("refresh failed");
            assert_eq!(
                cache
                    .blocking_scope("sms", Uuid::new_v4(), None)
                    .await
                    .as_deref(),
                Some(PLATFORM_SCOPE)
            );
        }

        assert_eq!(release(&first, id).await, "released");

        drop(first_listener);
        drop(second_listener);
        first.cleanup().await;
        second.cleanup().await;
    })
    .await;
}

// --- release drain ---------------------------------------------------------

async fn write_outbox_row(
    tenant: &TestTenant,
    vault: &VaultKeyStore,
    cache: &KeyCache,
    campaign_id: Option<&str>,
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
    let destination =
        encryption::encrypt(&dek, aad, b"+15550400").expect("encrypt failed");
    let payload =
        encryption::encrypt(&dek, aad, b"hello there").expect("encrypt failed");
    let class = if campaign_id.is_some() {
        "marketing"
    } else {
        "transactional"
    };

    insert_transactional(
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
        b"unused-hmac",
        &destination,
        &payload,
        Uuid::new_v4(),
        Uuid::new_v4(),
        None,
        None,
    )
    .await
    .expect("insert_transactional failed");

    comms_request_id
}

async fn engage_tenant_kill_switch(
    pool: &PgPool,
    scope_value: &str,
    scope_key: Option<&str>,
) -> KillSwitch {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO kill_switch (id, scope, scope_key, on_queued, engaged_by, engaged_at, reason) \
         VALUES ($1, $2, $3, 'hold', 'test-actor', now(), 'test')",
    )
    .bind(id)
    .bind(scope_value)
    .bind(scope_key)
    .execute(pool)
    .await
    .expect("engaging tenant switch failed");
    sqlx::query_as::<_, KillSwitch>(
        "SELECT id, scope, scope_key, on_queued, engaged_by, engaged_at, reason, \
                released_by, released_at FROM kill_switch WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("reading back the switch failed")
}

async fn release_tenant_kill_switch(pool: &PgPool, switch: KillSwitch) -> KillSwitch {
    sqlx::query("UPDATE kill_switch SET released_by = 'test-actor', released_at = now() WHERE id = $1")
        .bind(switch.id)
        .execute(pool)
        .await
        .expect("releasing tenant switch failed");
    KillSwitch {
        released_at: Some(Utc::now()),
        ..switch
    }
}

async fn final_status(pool: &PgPool, comms_request_id: Uuid) -> Option<String> {
    sqlx::query_scalar("SELECT final_status FROM comms_request WHERE id = $1")
        .bind(comms_request_id)
        .fetch_one(pool)
        .await
        .expect("comms_request row must exist")
}

async fn dispatcher_context(
    tenant: &TestTenant,
    cache: KeyCache,
    kill_switches: Arc<KillSwitchCache>,
) -> (Arc<DispatcherContext>, MockServer) {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message_id": "msg-platform-drain",
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
        kill_switches,
        draining: Arc::new(std::sync::RwLock::new(HashMap::new())),
        quota_day_boundary_tz: "UTC".to_string(),
        quota: Arc::new(QuotaTracker::new("UTC")),
    });
    (ctx, mock_server)
}

fn small_cache() -> KeyCache {
    KeyCache::new(NonZeroUsize::new(8).unwrap(), Duration::from_secs(60))
}

/// T-058 decision 5, folding in T-016's own drain bug: a released switch's
/// drain must not send rows another switch still holds.
#[tokio::test]
async fn release_drain_skips_rows_a_still_engaged_campaign_switch_holds() {
    isolated(|| async {
        let vault = vault_keystore();
        let tenant = provision_test_tenant(&vault).await;
        let cache = small_cache();

        let plain_id = write_outbox_row(&tenant, &vault, &cache, None).await;
        let held_id =
            write_outbox_row(&tenant, &vault, &cache, Some("spring-sale")).await;

        let global =
            engage_tenant_kill_switch(&tenant.tenant_pool, tenant_scope::GLOBAL, None)
                .await;
        let campaign = engage_tenant_kill_switch(
            &tenant.tenant_pool,
            tenant_scope::CAMPAIGN,
            Some("spring-sale"),
        )
        .await;
        let released = release_tenant_kill_switch(&tenant.tenant_pool, global).await;

        let kill_switches = Arc::new(KillSwitchCache::new());
        kill_switches
            .refresh(&tenant.tenant_pool)
            .await
            .expect("refresh failed");
        let (ctx, _mock) = dispatcher_context(&tenant, cache, kill_switches).await;

        drain_released_scope(ctx, "sms".to_string(), released, 10).await;

        assert_eq!(
            final_status(&tenant.tenant_pool, plain_id).await.as_deref(),
            Some("sent")
        );
        assert_eq!(
            final_status(&tenant.tenant_pool, held_id).await,
            None,
            "the still-engaged campaign switch's row must stay held"
        );

        release_tenant_kill_switch(&tenant.tenant_pool, campaign).await;
        tenant.cleanup().await;
    })
    .await;
}

/// A platform switch overrides a tenant's, never the reverse: releasing the
/// tenant's own switch must not drain anything past a platform suspension.
#[tokio::test]
async fn release_drain_sends_nothing_while_a_platform_switch_holds_the_tenant() {
    isolated(|| async {
        let vault = vault_keystore();
        let tenant = provision_test_tenant(&vault).await;
        let cache = small_cache();

        let row_id = write_outbox_row(&tenant, &vault, &cache, None).await;
        let global =
            engage_tenant_kill_switch(&tenant.tenant_pool, tenant_scope::GLOBAL, None)
                .await;
        engage(&tenant, scope::TENANT, Some(tenant.tenant_id))
            .await
            .expect("engaging platform switch failed");
        let released = release_tenant_kill_switch(&tenant.tenant_pool, global).await;

        let kill_switches = Arc::new(KillSwitchCache::new());
        kill_switches
            .refresh_with_platform(&tenant.tenant_pool, tenant.platform_source())
            .await
            .expect("refresh failed");
        let (ctx, _mock) = dispatcher_context(&tenant, cache, kill_switches).await;

        drain_released_scope(ctx, "sms".to_string(), released, 10).await;

        assert_eq!(final_status(&tenant.tenant_pool, row_id).await, None);
        let leased: Option<chrono::DateTime<Utc>> = sqlx::query_scalar(
            "SELECT leased_until FROM outbox WHERE comms_request_id = $1",
        )
        .bind(row_id)
        .fetch_one(&tenant.tenant_pool)
        .await
        .expect("outbox row must still exist");
        assert_eq!(leased, None, "the held row must not even be leased");

        tenant.cleanup().await;
    })
    .await;
}

// --- ingest ----------------------------------------------------------------

/// `messgr-ingest` never sees the fan-out `NOTIFY` (PgBouncer, §2.3): its
/// per-tenant poll, started by `TenantRegistry::get_or_open`, is its only
/// path to a platform switch.
#[tokio::test]
async fn the_ingest_registry_poll_picks_up_a_platform_switch() {
    isolated(|| async {
        let vault = vault_keystore();
        let tenant = provision_test_tenant(&vault).await;
        set_tenant_config(
            &tenant.control_pool,
            &tenant.control_url,
            &tenant.slug,
            TenantConfigInput {
                retention_years: 7,
                default_timezone: "UTC".to_string(),
                default_locale: "en".to_string(),
                schedule_horizon_days: 90,
                quota_day_boundary_tz: "UTC".to_string(),
                verification_mode: verification_mode::OBSERVE.to_string(),
                kill_switch_release_rate: 500,
                reconcile_attempts_cap: 5,
            },
            "test-actor",
        )
        .await
        .expect("setting tenant config failed");

        engage(&tenant, scope::TENANT, Some(tenant.tenant_id))
            .await
            .expect("engage failed");

        let registry = TenantRegistry::new();
        let context = registry
            .get_or_open(
                &tenant.control_pool,
                &tenant.control_url,
                &vault,
                tenant.tenant_id,
                2,
            )
            .await
            .expect("opening tenant context failed");

        let deadline = tokio::time::Instant::now() + NOTIFY_TIMEOUT;
        loop {
            let scope = context
                .kill_switches
                .blocking_scope("sms", Uuid::new_v4(), None)
                .await;
            if scope.as_deref() == Some(PLATFORM_SCOPE) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "ingest's poll never reported the platform switch"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        registry.evict_idle(Duration::ZERO).await;
        tenant.cleanup().await;
    })
    .await;
}

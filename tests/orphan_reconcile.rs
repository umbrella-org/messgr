//! `orphan_reconcile` integration suite (DESIGN.md §4.4/§10, T-022, T-030),
//! following `tests/partition_lifecycle.rs`/`tests/keystore.rs` conventions:
//! real tenant provisioning, real Vault dev-mode Transit, no mocks.

use chrono::Utc;
use sqlx::PgPool;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Once;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, SubscriberExt};
use uuid::Uuid;

use messgr::db;
use messgr::encryption;
use messgr::keystore::VaultKeyStore;
use messgr::orphan_reconcile::reconcile::run_for_tenant;
use messgr::profile::Profile;
use messgr::tenant::pool::connect_tenant_pool;
use messgr::tenant::provision::provision_tenant;
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
    control_url: String,
    tenant_pool: PgPool,
    slug: String,
    db_name: String,
}

impl TestTenant {
    async fn provision(prefix: &str) -> Self {
        let control_url = control_database_url();
        let control_pool = db::connect(&control_url, 5)
            .await
            .expect("failed to connect to control database");
        let vault = vault_keystore();

        let slug = unique_name(&format!("test_or_{prefix}"));
        let db_name = unique_name(&format!("test_db_or_{prefix}"));
        let tenant_id = provision_tenant(
            &control_pool,
            &control_url,
            &slug,
            "eu",
            &db_name,
            "test-actor",
            vault.client(),
        )
        .await
        .expect("provisioning test tenant failed")
        .tenant_id;

        let tenant_pool =
            connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 5)
                .await
                .expect("connecting tenant pool failed")
                .pool;

        TestTenant {
            control_pool,
            control_url,
            tenant_pool,
            slug,
            db_name,
        }
    }

    async fn teardown(self) {
        self.tenant_pool.close().await;
        drop_test_tenant(&self.control_pool, &self.db_name, &self.slug).await;
    }
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

#[allow(clippy::too_many_arguments)]
async fn insert_orphan_event(
    pool: &PgPool,
    id: Uuid,
    provider_ref: &str,
    event_type: &str,
    occurred_at: chrono::DateTime<Utc>,
    payload: Option<serde_json::Value>,
    reconcile_attempts: i16,
) {
    sqlx::query(
        r#"
        INSERT INTO orphan_event (
            id, received_at, provider, provider_ref, occurred_at, event_type,
            provider_status, provider_payload_raw, reconcile_attempts
        ) VALUES ($1, now(), 'twilio', $2, $3, $4, 'DELIVERED', $5, $6)
        "#,
    )
    .bind(id)
    .bind(provider_ref)
    .bind(occurred_at)
    .bind(event_type)
    .bind(payload)
    .bind(reconcile_attempts)
    .execute(pool)
    .await
    .expect("inserting orphan_event failed");
}

async fn set_reconcile_attempts_cap(tenant: &TestTenant, cap: i16) {
    let input = TenantConfigInput {
        retention_years: 7,
        default_timezone: "Europe/London".to_string(),
        default_locale: "en-GB".to_string(),
        schedule_horizon_days: 90,
        quota_day_boundary_tz: "Europe/London".to_string(),
        verification_mode: verification_mode::OBSERVE.to_string(),
        kill_switch_release_rate: 500,
        reconcile_attempts_cap: cap,
    };
    set_tenant_config(
        &tenant.control_pool,
        &tenant.control_url,
        &tenant.slug,
        input,
        "test-actor",
    )
    .await
    .expect("setting tenant_config failed");
}

#[derive(Default)]
struct FieldMap(HashMap<String, String>);

impl Visit for FieldMap {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .entry(field.name().to_string())
            .or_insert_with(|| format!("{value:?}"));
    }
}

thread_local! {
    static CAPTURED_WARN_EVENTS: RefCell<Vec<HashMap<String, String>>> = const { RefCell::new(Vec::new()) };
}

struct CapturingLayer;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CapturingLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if *event.metadata().level() != tracing::Level::WARN {
            return;
        }
        let mut fields = FieldMap::default();
        event.record(&mut fields);
        CAPTURED_WARN_EVENTS.with(|events| events.borrow_mut().push(fields.0));
    }
}

static INIT_WARN_CAPTURE: Once = Once::new();

/// Installs a single process-wide default subscriber, once, that captures
/// every WARN-level event's fields into a thread-local buffer (T-035 rework,
/// F1). A per-test `tracing::subscriber::set_default` looked simpler, but
/// `reconcile.rs`'s shared `tracing::warn!` call site only ever decides its
/// cached `Interest` once, lazily, the first time it fires anywhere in the
/// process (`tracing_core::callsite::DefaultCallsite::register`) -- and that
/// one-time decision reads whichever thread's dispatcher happens to be
/// current *at that instant*. If the very first test to hit this line has no
/// subscriber installed (the global no-op default), the callsite is cached
/// `never` for the rest of the process, and no *other* test's own
/// `set_default` call rebuilds this specific callsite's cache to fix it --
/// `tracing-core`'s new-`Dispatch` rebuild does walk every *already
/// registered* callsite, but only ones some test has already hit at least
/// once. Reproduced live, roughly 1-in-10 runs under the default parallel
/// test harness. Installing exactly one subscriber for the whole test
/// binary's lifetime side-steps this: whichever test's `run_for_tenant` call
/// happens to hit the line first, it always finds this permanently-installed
/// subscriber already current (never the no-op default), so the callsite's
/// one-time decision is always correct; even if some other, non-participating
/// test raced ahead and had already cached it `never` before this subscriber
/// was installed, the `Dispatch::new()` call inside `set_global_default`
/// walks every already-registered callsite (this one included, by then) and
/// recomputes it against the newly-installed subscriber, correcting it.
fn init_warn_capture() {
    INIT_WARN_CAPTURE.call_once(|| {
        let subscriber = tracing_subscriber::registry().with(CapturingLayer);
        tracing::subscriber::set_global_default(subscriber)
            .expect("installing the test-wide WARN-capturing subscriber failed");
    });
}

/// Clears this thread's captured WARN events (T-035) -- call immediately
/// before the call under test. Threads are reused across tests by the
/// default harness, so a stale event from an earlier test that happened to
/// run on this same thread would otherwise still be sitting in the buffer.
fn reset_warn_capture() {
    init_warn_capture();
    CAPTURED_WARN_EVENTS.with(|events| events.borrow_mut().clear());
}

/// Snapshots this thread's captured WARN events since the last
/// `reset_warn_capture()` call (T-035). Captures are thread-local and
/// `#[tokio::test]`'s single-threaded runtime keeps a whole test on one OS
/// thread, so no test sees another's events.
fn captured_warn_events() -> Vec<HashMap<String, String>> {
    CAPTURED_WARN_EVENTS.with(|events| events.borrow().clone())
}

#[tokio::test]
async fn match_and_promote_encrypts_and_advances_final_status() {
    let tenant = TestTenant::provision("promote").await;
    let vault = vault_keystore();

    let comms_request_id = Uuid::new_v4();
    let customer_id = Uuid::new_v4();
    let created_at = Utc::now();
    let occurred_at = Utc::now();
    let payload = serde_json::json!({"status": "delivered"});

    insert_comms_request(
        &tenant.tenant_pool,
        comms_request_id,
        customer_id,
        created_at,
        Some("sent"),
    )
    .await;
    insert_comms_event(
        &tenant.tenant_pool,
        comms_request_id,
        customer_id,
        occurred_at,
        "abc",
    )
    .await;
    let orphan_id = Uuid::new_v4();
    insert_orphan_event(
        &tenant.tenant_pool,
        orphan_id,
        "abc",
        "delivered",
        occurred_at,
        Some(payload.clone()),
        0,
    )
    .await;

    let report = run_for_tenant(
        &tenant.control_pool,
        &tenant.control_url,
        &tenant.slug,
        &vault,
        5,
    )
    .await
    .expect("reconcile run failed");
    assert_eq!(report.reconciled, 1);
    assert_eq!(report.aged_out, 0);
    assert_eq!(report.still_pending, 0);

    let orphan_remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM orphan_event")
        .fetch_one(&tenant.tenant_pool)
        .await
        .expect("counting orphan_event rows failed");
    assert_eq!(
        orphan_remaining, 0,
        "the promoted orphan_event row must be deleted"
    );

    let (ciphertext, final_status): (Option<Vec<u8>>, Option<String>) = sqlx::query_as(
        r#"
        SELECT ce.provider_payload_ciphertext, cr.final_status
        FROM comms_event ce
        JOIN comms_request cr ON cr.id = ce.comms_request_id
        WHERE ce.event_type = 'delivered'
        "#,
    )
    .fetch_one(&tenant.tenant_pool)
    .await
    .expect("fetching promoted comms_event failed");

    let ciphertext =
        ciphertext.expect("promoted comms_event must carry a non-NULL ciphertext");
    let dek = messgr::customer_dek::lifecycle::get_or_create_dek(
        &tenant.tenant_pool,
        &vault,
        &messgr::key_cache::KeyCache::new(
            std::num::NonZeroUsize::new(1).unwrap(),
            std::time::Duration::from_secs(60),
        ),
        &format!("transit/{}", tenant.slug),
        customer_id,
    )
    .await
    .expect("fetching the customer's DEK for verification failed");
    let plaintext = encryption::decrypt(&dek, comms_request_id.as_bytes(), &ciphertext)
        .expect("decrypting the promoted ciphertext failed");
    let decoded: serde_json::Value = serde_json::from_slice(&plaintext)
        .expect("decrypted payload must be valid JSON");
    assert_eq!(decoded, payload);

    assert_eq!(
        final_status.as_deref(),
        Some("delivered"),
        "final_status must advance from sent to delivered"
    );

    tenant.teardown().await;
}

#[tokio::test]
async fn final_status_regression_is_guarded_against() {
    let tenant = TestTenant::provision("regression").await;
    let vault = vault_keystore();

    let comms_request_id = Uuid::new_v4();
    let customer_id = Uuid::new_v4();
    let created_at = Utc::now();
    let occurred_at = Utc::now();

    insert_comms_request(
        &tenant.tenant_pool,
        comms_request_id,
        customer_id,
        created_at,
        Some("delivered"),
    )
    .await;
    insert_comms_event(
        &tenant.tenant_pool,
        comms_request_id,
        customer_id,
        occurred_at,
        "xyz",
    )
    .await;
    insert_orphan_event(
        &tenant.tenant_pool,
        Uuid::new_v4(),
        "xyz",
        "sent",
        occurred_at,
        None,
        0,
    )
    .await;

    let report = run_for_tenant(
        &tenant.control_pool,
        &tenant.control_url,
        &tenant.slug,
        &vault,
        5,
    )
    .await
    .expect("reconcile run failed");
    assert_eq!(report.reconciled, 1);

    let final_status: Option<String> =
        sqlx::query_scalar("SELECT final_status FROM comms_request WHERE id = $1")
            .bind(comms_request_id)
            .fetch_one(&tenant.tenant_pool)
            .await
            .expect("fetching comms_request.final_status failed");
    assert_eq!(
        final_status.as_deref(),
        Some("delivered"),
        "a less-final event_type (sent) must not regress an already-more-final status (delivered)"
    );

    tenant.teardown().await;
}

#[tokio::test]
async fn no_match_under_cap_increments_attempts_and_keeps_the_row() {
    let tenant = TestTenant::provision("no_match").await;
    let vault = vault_keystore();

    let orphan_id = Uuid::new_v4();
    insert_orphan_event(
        &tenant.tenant_pool,
        orphan_id,
        "unmatched-ref",
        "delivered",
        Utc::now(),
        None,
        0,
    )
    .await;

    let report = run_for_tenant(
        &tenant.control_pool,
        &tenant.control_url,
        &tenant.slug,
        &vault,
        5,
    )
    .await
    .expect("reconcile run failed");
    assert_eq!(report.reconciled, 0);
    assert_eq!(report.aged_out, 0);
    assert_eq!(report.still_pending, 1);

    let reconcile_attempts: i16 =
        sqlx::query_scalar("SELECT reconcile_attempts FROM orphan_event WHERE id = $1")
            .bind(orphan_id)
            .fetch_one(&tenant.tenant_pool)
            .await
            .expect("fetching reconcile_attempts failed");
    assert_eq!(reconcile_attempts, 1);

    tenant.teardown().await;
}

#[tokio::test]
async fn no_match_at_cap_deletes_the_row() {
    let tenant = TestTenant::provision("cap").await;
    let vault = vault_keystore();

    let orphan_id = Uuid::new_v4();
    insert_orphan_event(
        &tenant.tenant_pool,
        orphan_id,
        "never-matches",
        "delivered",
        Utc::now(),
        None,
        4, // one below the cap of 5 -- this run's increment reaches it
    )
    .await;

    let report = run_for_tenant(
        &tenant.control_pool,
        &tenant.control_url,
        &tenant.slug,
        &vault,
        5,
    )
    .await
    .expect("reconcile run failed");
    assert_eq!(report.reconciled, 0);
    assert_eq!(report.aged_out, 1);
    assert_eq!(report.still_pending, 0);

    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM orphan_event WHERE id = $1")
            .bind(orphan_id)
            .fetch_one(&tenant.tenant_pool)
            .await
            .expect("counting remaining orphan_event rows failed");
    assert_eq!(
        remaining, 0,
        "a row past the reconcile_attempts cap must be deleted"
    );

    tenant.teardown().await;
}

// T-033: the reconcile-attempts cap now comes from tenant_config, not the
// hardcoded default -- prove a configured value actually gates aging-out.
#[tokio::test]
async fn configured_reconcile_attempts_cap_is_honored() {
    let tenant = TestTenant::provision("configured_cap").await;
    let vault = vault_keystore();

    set_reconcile_attempts_cap(&tenant, 2).await;

    let orphan_id = Uuid::new_v4();
    insert_orphan_event(
        &tenant.tenant_pool,
        orphan_id,
        "never-matches",
        "delivered",
        Utc::now(),
        None,
        1, // one below the configured cap of 2 -- this run's increment reaches it
    )
    .await;

    reset_warn_capture();
    let report = run_for_tenant(
        &tenant.control_pool,
        &tenant.control_url,
        &tenant.slug,
        &vault,
        5,
    )
    .await
    .expect("reconcile run failed");
    assert_eq!(report.reconciled, 0);
    assert_eq!(
        report.aged_out, 1,
        "the configured cap of 2, not the hardcoded default of 5, must gate aging-out"
    );
    assert_eq!(report.still_pending, 0);

    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM orphan_event WHERE id = $1")
            .bind(orphan_id)
            .fetch_one(&tenant.tenant_pool)
            .await
            .expect("counting remaining orphan_event rows failed");
    assert_eq!(
        remaining, 0,
        "a row past the configured reconcile_attempts cap must be deleted"
    );

    let events = captured_warn_events();
    let warn = events
        .iter()
        .find(|f| {
            f.get("message")
                .is_some_and(|m| m.contains("exceeded reconcile_attempts_cap"))
        })
        .expect("expected the age-out warn to fire");
    assert_eq!(
        warn.get("tenant_slug").map(String::as_str),
        Some(tenant.slug.as_str())
    );
    assert_eq!(
        warn.get("orphan_id").map(String::as_str),
        Some(orphan_id.to_string()).as_deref()
    );
    assert_eq!(
        warn.get("provider_ref").map(String::as_str),
        Some("never-matches")
    );
    assert_eq!(
        warn.get("event_type").map(String::as_str),
        Some("delivered")
    );

    tenant.teardown().await;
}

// T-033: an unconfigured tenant (no tenant_config row) must still fall back
// to the hardcoded default of 5.
#[tokio::test]
async fn unconfigured_tenant_still_ages_out_at_the_hardcoded_default() {
    let tenant = TestTenant::provision("unconfigured_cap").await;
    let vault = vault_keystore();

    let orphan_id = Uuid::new_v4();
    insert_orphan_event(
        &tenant.tenant_pool,
        orphan_id,
        "never-matches",
        "delivered",
        Utc::now(),
        None,
        4, // one below the hardcoded default of 5
    )
    .await;

    reset_warn_capture();
    let report = run_for_tenant(
        &tenant.control_pool,
        &tenant.control_url,
        &tenant.slug,
        &vault,
        5,
    )
    .await
    .expect("reconcile run failed");
    assert_eq!(report.aged_out, 1);

    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM orphan_event WHERE id = $1")
            .bind(orphan_id)
            .fetch_one(&tenant.tenant_pool)
            .await
            .expect("counting remaining orphan_event rows failed");
    assert_eq!(
        remaining, 0,
        "an unconfigured tenant must still age out at the hardcoded default of 5"
    );

    let events = captured_warn_events();
    let warn = events
        .iter()
        .find(|f| {
            f.get("message")
                .is_some_and(|m| m.contains("exceeded reconcile_attempts_cap"))
        })
        .expect("expected the age-out warn to fire");
    assert_eq!(
        warn.get("tenant_slug").map(String::as_str),
        Some(tenant.slug.as_str())
    );
    assert_eq!(
        warn.get("orphan_id").map(String::as_str),
        Some(orphan_id.to_string()).as_deref()
    );
    assert_eq!(
        warn.get("provider_ref").map(String::as_str),
        Some("never-matches")
    );
    assert_eq!(
        warn.get("event_type").map(String::as_str),
        Some("delivered")
    );

    tenant.teardown().await;
}

// T-034 finding F4: an orphan row carrying an event_type outside the
// documented comms_event/orphan_event set must never be promoted -- it
// should age out through the existing reconcile_attempts cap path exactly
// like a provider_ref that matches nothing.
#[tokio::test]
async fn unrecognized_event_type_ages_out_instead_of_promoting() {
    let tenant = TestTenant::provision("bad_type").await;
    let vault = vault_keystore();

    let comms_request_id = Uuid::new_v4();
    let customer_id = Uuid::new_v4();
    let created_at = Utc::now();
    let occurred_at = Utc::now();

    insert_comms_request(
        &tenant.tenant_pool,
        comms_request_id,
        customer_id,
        created_at,
        None,
    )
    .await;
    insert_comms_event(
        &tenant.tenant_pool,
        comms_request_id,
        customer_id,
        occurred_at,
        "abc",
    )
    .await;
    let orphan_id = Uuid::new_v4();
    insert_orphan_event(
        &tenant.tenant_pool,
        orphan_id,
        "abc",
        "made_up_status",
        occurred_at,
        None,
        0,
    )
    .await;

    let report = run_for_tenant(
        &tenant.control_pool,
        &tenant.control_url,
        &tenant.slug,
        &vault,
        5,
    )
    .await
    .expect("reconcile run failed");
    assert_eq!(
        report.reconciled, 0,
        "an unrecognized event_type must never be promoted"
    );
    assert_eq!(report.still_pending, 1);

    let reconcile_attempts: i16 =
        sqlx::query_scalar("SELECT reconcile_attempts FROM orphan_event WHERE id = $1")
            .bind(orphan_id)
            .fetch_one(&tenant.tenant_pool)
            .await
            .expect("fetching reconcile_attempts failed");
    assert_eq!(reconcile_attempts, 1);

    let comms_event_count: i64 = sqlx::query_scalar("SELECT count(*) FROM comms_event")
        .fetch_one(&tenant.tenant_pool)
        .await
        .expect("counting comms_event rows failed");
    assert_eq!(
        comms_event_count, 1,
        "no new comms_event row should have been written for the unrecognized orphan"
    );

    tenant.teardown().await;
}

// T-034 finding F5: when more than one comms_event row shares a
// provider_ref, find_match must deterministically pick the most recently
// occurred one rather than leaving the choice to Postgres' LIMIT 1.
#[tokio::test]
async fn multi_match_resolves_to_the_most_recently_occurred_row() {
    let tenant = TestTenant::provision("multi_match").await;
    let vault = vault_keystore();

    let shared_provider_ref = "shared-ref";

    let older_request_id = Uuid::new_v4();
    let older_customer_id = Uuid::new_v4();
    let older_created_at = Utc::now() - chrono::Duration::minutes(10);
    let older_occurred_at = Utc::now() - chrono::Duration::minutes(10);
    insert_comms_request(
        &tenant.tenant_pool,
        older_request_id,
        older_customer_id,
        older_created_at,
        None,
    )
    .await;
    insert_comms_event(
        &tenant.tenant_pool,
        older_request_id,
        older_customer_id,
        older_occurred_at,
        shared_provider_ref,
    )
    .await;

    let newer_request_id = Uuid::new_v4();
    let newer_customer_id = Uuid::new_v4();
    let newer_created_at = Utc::now();
    let newer_occurred_at = Utc::now();
    insert_comms_request(
        &tenant.tenant_pool,
        newer_request_id,
        newer_customer_id,
        newer_created_at,
        None,
    )
    .await;
    insert_comms_event(
        &tenant.tenant_pool,
        newer_request_id,
        newer_customer_id,
        newer_occurred_at,
        shared_provider_ref,
    )
    .await;

    insert_orphan_event(
        &tenant.tenant_pool,
        Uuid::new_v4(),
        shared_provider_ref,
        "delivered",
        Utc::now(),
        None,
        0,
    )
    .await;

    let report = run_for_tenant(
        &tenant.control_pool,
        &tenant.control_url,
        &tenant.slug,
        &vault,
        5,
    )
    .await
    .expect("reconcile run failed");
    assert_eq!(report.reconciled, 1);

    let newer_final_status: Option<String> =
        sqlx::query_scalar("SELECT final_status FROM comms_request WHERE id = $1")
            .bind(newer_request_id)
            .fetch_one(&tenant.tenant_pool)
            .await
            .expect("fetching the newer request's final_status failed");
    assert_eq!(
        newer_final_status.as_deref(),
        Some("delivered"),
        "the match must resolve to the comms_event row with the later occurred_at"
    );

    let older_final_status: Option<String> =
        sqlx::query_scalar("SELECT final_status FROM comms_request WHERE id = $1")
            .bind(older_request_id)
            .fetch_one(&tenant.tenant_pool)
            .await
            .expect("fetching the older request's final_status failed");
    assert_eq!(
        older_final_status, None,
        "the older comms_event row sharing the provider_ref must not be promoted against"
    );

    tenant.teardown().await;
}

// T-030 review finding F1: an orphan row whose own provider_ref is empty
// must never match against the '' every dispatch-internal comms_event row
// carries by default -- that is a sentinel, not a real reference, and
// matching on it would promote an unrelated stranger's request.
#[tokio::test]
async fn empty_provider_ref_never_matches_the_dispatch_internal_sentinel() {
    let tenant = TestTenant::provision("empty_ref").await;
    let vault = vault_keystore();

    // A real customer's request, with a dispatch-internal comms_event row
    // (provider_ref defaults to '' for these -- migration 0004).
    let victim_request_id = Uuid::new_v4();
    let victim_customer_id = Uuid::new_v4();
    let created_at = Utc::now();
    insert_comms_request(
        &tenant.tenant_pool,
        victim_request_id,
        victim_customer_id,
        created_at,
        None,
    )
    .await;
    sqlx::query(
        "INSERT INTO comms_event (comms_request_id, customer_id, occurred_at, event_type) \
         VALUES ($1, $2, $3, 'queued')",
    )
    .bind(victim_request_id)
    .bind(victim_customer_id)
    .bind(Utc::now())
    .execute(&tenant.tenant_pool)
    .await
    .expect("inserting the victim's dispatch-internal comms_event failed");

    // An unrelated orphan row that itself ends up with an empty
    // provider_ref (a malformed receipt, say).
    let orphan_id = Uuid::new_v4();
    insert_orphan_event(
        &tenant.tenant_pool,
        orphan_id,
        "",
        "delivered",
        Utc::now(),
        Some(serde_json::json!({"secret": "unrelated-stranger-payload"})),
        0,
    )
    .await;

    let report = run_for_tenant(
        &tenant.control_pool,
        &tenant.control_url,
        &tenant.slug,
        &vault,
        5,
    )
    .await
    .expect("reconcile run failed");
    assert_eq!(
        report.reconciled, 0,
        "an empty provider_ref must never be treated as a match"
    );
    assert_eq!(report.still_pending, 1);

    let victim_final_status: Option<String> =
        sqlx::query_scalar("SELECT final_status FROM comms_request WHERE id = $1")
            .bind(victim_request_id)
            .fetch_one(&tenant.tenant_pool)
            .await
            .expect("fetching the victim's final_status failed");
    assert_eq!(
        victim_final_status, None,
        "the unrelated victim's final_status must be untouched"
    );

    let orphan_still_present: i64 =
        sqlx::query_scalar("SELECT count(*) FROM orphan_event WHERE id = $1")
            .bind(orphan_id)
            .fetch_one(&tenant.tenant_pool)
            .await
            .expect("counting the orphan row failed");
    assert_eq!(
        orphan_still_present, 1,
        "the orphan row must remain pending, not be wrongly promoted"
    );

    tenant.teardown().await;
}

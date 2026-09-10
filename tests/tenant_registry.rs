//! `TenantRegistry` idle-eviction integration suite (T-031), following
//! `tests/kill_switch.rs`/`tests/ingest.rs`'s conventions: real provisioning
//! against the local stack, no mocks. Drives `TenantRegistry` directly — no
//! `axum` server needed, since eviction is internal to the registry.

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use uuid::Uuid;

use messgr::db;
use messgr::keystore::VaultKeyStore;
use messgr::profile::Profile;
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

fn sample_tenant_config() -> TenantConfigInput {
    TenantConfigInput {
        retention_years: 7,
        default_timezone: "UTC".to_string(),
        default_locale: "en".to_string(),
        schedule_horizon_days: 90,
        quota_day_boundary_tz: "UTC".to_string(),
        verification_mode: verification_mode::OBSERVE.to_string(),
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

/// Proves eviction actually cancels the per-tenant kill-switch poll loop,
/// not just the registry's own reference to the context (T-031): a TTL of
/// zero makes the just-opened entry immediately eligible, and `evict_idle`
/// only returns after the cancelled poll task's `JoinHandle` completes — a
/// broken cancellation wire-up would hang here, not silently pass.
#[tokio::test]
async fn evicts_an_idle_tenant_and_stops_its_poll_loop() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("connecting to control database failed");
    let vault = vault_keystore();

    let slug = unique_name("test_tenant_registry");
    let database_name = unique_name("test_db_registry");

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

    set_tenant_config(
        &control_pool,
        &control_url,
        &slug,
        sample_tenant_config(),
        "test-actor",
    )
    .await
    .expect("setting tenant config failed");

    let registry = Arc::new(TenantRegistry::new());

    let first = registry
        .get_or_open(
            &control_pool,
            &control_url,
            &vault,
            provision_outcome.tenant_id,
            5,
        )
        .await
        .expect("opening tenant context failed");

    tokio::time::timeout(
        Duration::from_secs(5),
        registry.evict_idle(Duration::from_millis(0)),
    )
    .await
    .expect("evict_idle must not hang if its poll loop actually cancels");

    let second = registry
        .get_or_open(
            &control_pool,
            &control_url,
            &vault,
            provision_outcome.tenant_id,
            5,
        )
        .await
        .expect("reopening the evicted tenant context failed");

    assert!(
        !Arc::ptr_eq(&first, &second),
        "an evicted tenant must reopen a fresh context, not reuse the evicted one"
    );

    drop(first);
    drop(second);
    drop_test_tenant(&control_pool, &database_name, &slug).await;
}

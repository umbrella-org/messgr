//! T-039: dispatcher leader election, over a real Postgres tenant database
//! (`tests/tenancy.rs`/`tests/dispatcher.rs`'s no-mocks convention) — proves
//! build-order.md's own acceptance bar: "Dispatcher leader election holds
//! under a forced failover, over a direct connection, with exactly one
//! active dispatcher observed throughout (§2.3)."

use std::time::Duration;

use sqlx::{Connection, PgPool};
use uuid::Uuid;

use messgr::db;
use messgr::dispatcher::leader;
use messgr::keystore::VaultKeyStore;
use messgr::profile::Profile;
use messgr::tenant::provision::provision_tenant;

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
    slug: String,
    database_name: String,
}

async fn provision_test_tenant(vault: &VaultKeyStore) -> TestTenant {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");

    let slug = unique_name("test_leader_election");
    let database_name = unique_name("test_db_leader_election");

    provision_tenant(
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

    TestTenant {
        control_pool,
        slug,
        database_name,
    }
}

impl TestTenant {
    fn connect_options(&self) -> sqlx::postgres::PgConnectOptions {
        db::with_database_name(&control_database_url(), &self.database_name)
            .expect("deriving leader-lock connection options failed")
    }

    async fn cleanup(self) {
        drop_test_tenant(&self.control_pool, &self.database_name, &self.slug).await;
    }
}

#[tokio::test]
async fn two_processes_contend_and_exactly_one_holds_leadership() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let options = tenant.connect_options();

    // Spawn order is not race order -- either task may actually win the
    // underlying `pg_try_advisory_lock`, so `select!` on both and treat
    // whichever resolves first as the winner, rather than assuming it's the
    // one spawned first.
    let mut task_a = tokio::spawn(leader::acquire(options.clone()));
    let mut task_b = tokio::spawn(leader::acquire(options.clone()));

    let (winner_leadership, loser) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            result = &mut task_a => (result.expect("winning task panicked"), task_b),
            result = &mut task_b => (result.expect("winning task panicked"), task_a),
        }
    })
    .await
    .expect("neither task acquired leadership in time");

    let loser_still_pending =
        tokio::time::timeout(Duration::from_millis(500), loser).await;
    assert!(
        loser_still_pending.is_err(),
        "the losing task must not resolve while the winner holds leadership"
    );

    let mut independent_conn = sqlx::postgres::PgConnection::connect_with(&options)
        .await
        .expect("opening an independent verification connection failed");
    let independently_observed_free: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_lock(1)")
            .fetch_one(&mut independent_conn)
            .await
            .expect("independent pg_try_advisory_lock query failed");
    assert!(
        !independently_observed_free,
        "a fresh connection must also observe the lock held, not just the two contending tasks"
    );

    drop(winner_leadership);
    drop(independent_conn);
    tenant.cleanup().await;
}

#[tokio::test]
async fn standby_takes_over_within_seconds_of_a_forced_failover() {
    let vault = vault_keystore();
    let tenant = provision_test_tenant(&vault).await;
    let options = tenant.connect_options();

    let leader = leader::acquire(options.clone()).await;
    let standby = tokio::spawn(leader::acquire(options.clone()));

    drop(leader);

    let took_over = tokio::time::timeout(leader::RETRY_INTERVAL * 3, standby)
        .await
        .expect("standby did not take over within a forced-failover window")
        .expect("standby acquire task panicked");

    drop(took_over);
    tenant.cleanup().await;
}

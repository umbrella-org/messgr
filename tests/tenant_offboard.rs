//! Proves `tenant::offboard::destroy_tenant` end to end (T-059): a
//! provisioned tenant's database and Vault Transit mount are both gone after
//! one call, its `tenant` row reads `offboarding_destroy`, and a second call
//! against the same tenant does not error.

use uuid::Uuid;

use messgr::db;
use messgr::keystore::VaultKeyStore;
use messgr::profile::Profile;
use messgr::tenant::model::status;
use messgr::tenant::offboard::destroy_tenant;
use messgr::tenant::provision::provision_tenant;
use messgr::tenant::repo::find_by_id;
use sqlx::PgPool;

fn control_database_url() -> String {
    dotenvy::dotenv().ok();
    std::env::var("CONTROL_DATABASE_URL")
        .expect("CONTROL_DATABASE_URL must be set for tests")
}

fn unique_name(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

async fn database_exists(control_pool: &PgPool, database_name: &str) -> bool {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)",
    )
    .bind(database_name)
    .fetch_one(control_pool)
    .await
    .expect("checking pg_database failed")
}

#[tokio::test]
async fn destroy_tenant_drops_the_database_and_marks_the_tenant_destroyed() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let admin = VaultKeyStore::connect(Profile::Dev)
        .expect("connecting to dev-mode Vault failed");

    let slug = unique_name("test_tenant_offboard_destroy");
    let db_name = unique_name("test_db_offboard_destroy");

    let outcome = provision_tenant(
        &control_pool,
        &control_url,
        &slug,
        "eu",
        &db_name,
        "test-actor",
        admin.client(),
    )
    .await
    .expect("provisioning failed");

    assert!(
        database_exists(&control_pool, &db_name).await,
        "precondition: the tenant's database must exist before destroying it"
    );

    destroy_tenant(
        &control_pool,
        outcome.tenant_id,
        "test-actor",
        admin.client(),
    )
    .await
    .expect("destroy_tenant failed");

    assert!(
        !database_exists(&control_pool, &db_name).await,
        "destroy_tenant must drop the tenant's database"
    );

    let tenant = find_by_id(&control_pool, outcome.tenant_id)
        .await
        .expect("looking up the tenant failed")
        .expect("the tenant row must still exist after destroy (only its database is dropped)");
    assert_eq!(tenant.status, status::OFFBOARDING_DESTROY);

    let mounts = vaultrs::sys::mount::list(admin.client())
        .await
        .expect("listing mounts failed");
    assert!(
        !mounts.contains_key(&format!("transit/{slug}/")),
        "destroy_tenant must unmount the tenant's Transit engine"
    );

    // Best-effort teardown of the control-plane rows destroy_tenant leaves
    // behind (it only drops the tenant's own database, not its control-plane
    // bookkeeping), matching tests/tenant_vault.rs's own cleanup shape.
    let _ = sqlx::query("DELETE FROM platform_audit WHERE tenant_id = $1")
        .bind(outcome.tenant_id)
        .execute(&control_pool)
        .await;
    let _ = sqlx::query("DELETE FROM tenant WHERE id = $1")
        .bind(outcome.tenant_id)
        .execute(&control_pool)
        .await;
}

#[tokio::test]
async fn destroy_tenant_is_idempotent() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let admin = VaultKeyStore::connect(Profile::Dev)
        .expect("connecting to dev-mode Vault failed");

    let slug = unique_name("test_tenant_offboard_idempotent");
    let db_name = unique_name("test_db_offboard_idempotent");

    let outcome = provision_tenant(
        &control_pool,
        &control_url,
        &slug,
        "eu",
        &db_name,
        "test-actor",
        admin.client(),
    )
    .await
    .expect("provisioning failed");

    destroy_tenant(
        &control_pool,
        outcome.tenant_id,
        "test-actor",
        admin.client(),
    )
    .await
    .expect("first destroy_tenant call failed");
    destroy_tenant(
        &control_pool,
        outcome.tenant_id,
        "test-actor",
        admin.client(),
    )
    .await
    .expect(
        "second destroy_tenant call against an already-destroyed tenant must not error",
    );

    let _ = sqlx::query("DELETE FROM platform_audit WHERE tenant_id = $1")
        .bind(outcome.tenant_id)
        .execute(&control_pool)
        .await;
    let _ = sqlx::query("DELETE FROM tenant WHERE id = $1")
        .bind(outcome.tenant_id)
        .execute(&control_pool)
        .await;
}

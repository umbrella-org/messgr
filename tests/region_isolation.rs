//! Region-boundary isolation (decision 15, T-060): two independent control
//! databases standing in for two regions. Proves the boot-time region
//! assertion catches a process wired to the wrong region's control database,
//! and that a platform kill switch engaged in one region cannot reach a
//! tenant of the other.
//!
//! Both control databases live on the one local/CI Postgres cluster — what
//! makes them separate regions is that nothing in either references the
//! other, which is exactly the property under test. `compose.region-b.yml`
//! is the manual, separate-cluster version of the same proof.

use sqlx::PgPool;
use uuid::Uuid;

use messgr::db;
use messgr::platform_kill_switch::configure::engage_tx;
use messgr::platform_kill_switch::model::scope;
use messgr::platform_kill_switch::repo::list_active_for_tenant;
use messgr::tenant::repo::{assert_region, insert_provisioning, mark_active};

fn control_database_url() -> String {
    dotenvy::dotenv().ok();
    std::env::var("CONTROL_DATABASE_URL")
        .expect("CONTROL_DATABASE_URL must be set for tests")
}

/// A throwaway, fully migrated control database holding one active tenant
/// in `region`.
struct Region {
    pool: PgPool,
    database_name: String,
    tenant_id: Uuid,
}

impl Region {
    async fn create(admin: &PgPool, region: &str) -> Self {
        let database_name = format!("test_region_ctl_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE \"{database_name}\""))
            .execute(admin)
            .await
            .expect("creating throwaway control database failed");

        let options = db::with_database_name(&control_database_url(), &database_name)
            .expect("building throwaway control database URL failed");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .expect("connecting throwaway control database failed");
        sqlx::migrate!("./migrations/control")
            .run(&pool)
            .await
            .expect("migrating throwaway control database failed");

        let tenant_id = Uuid::new_v4();
        let slug = format!("test_region_{}", tenant_id.simple());
        insert_provisioning(
            &pool,
            tenant_id,
            &slug,
            region,
            &format!("{slug}_db"),
            &format!("transit/{slug}"),
            &Uuid::new_v4().to_string(),
        )
        .await
        .expect("inserting tenant row failed");
        // Active, so region A's platform fan-out would include it.
        mark_active(&pool, tenant_id)
            .await
            .expect("marking tenant active failed");

        Self {
            pool,
            database_name,
            tenant_id,
        }
    }

    async fn drop(self, admin: &PgPool) {
        self.pool.close().await;
        let _ = sqlx::query(&format!(
            "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
            self.database_name
        ))
        .execute(admin)
        .await;
    }
}

#[tokio::test]
async fn region_boundary_holds_between_two_control_databases() {
    let admin = db::connect(&control_database_url(), 2)
        .await
        .expect("connecting the control pool failed");
    let a = Region::create(&admin, "eu").await;
    let b = Region::create(&admin, "region-b").await;

    // Each region's processes boot cleanly against their own control DB.
    assert_region(&a.pool, "eu")
        .await
        .expect("region A check failed");
    assert_region(&b.pool, "region-b")
        .await
        .expect("region B check failed");

    // A region-A process wired to region B's control DB must refuse to boot.
    let wrong = b.pool.clone();
    let join_error = tokio::spawn(async move { assert_region(&wrong, "eu").await })
        .await
        .expect_err("assert_region must panic on another region's tenant");
    let payload = join_error.into_panic();
    let message = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
        .expect("panic payload was not a string message");
    assert!(
        message.contains("region mismatch") && message.contains("\"region-b\""),
        "panicked, but not with the region-mismatch message: {message:?}"
    );

    // T-058: a region-wide platform switch engaged in A neither blocks nor
    // NOTIFYs B's tenant.
    let mut tx = a.pool.begin().await.expect("begin failed");
    let outcome =
        engage_tx(&mut tx, scope::PLATFORM, None, "T-060 test", "test-region")
            .await
            .expect("engaging platform switch in region A failed");
    tx.commit().await.expect("commit failed");
    assert!(
        outcome.notify_targets.iter().all(|t| t.id != b.tenant_id)
            && outcome.notify_targets.iter().any(|t| t.id == a.tenant_id),
        "region A's fan-out must reach its own tenant and never region B's"
    );
    assert_eq!(
        list_active_for_tenant(&a.pool, a.tenant_id)
            .await
            .unwrap()
            .len(),
        1,
        "the switch must block region A's tenant"
    );
    assert!(
        list_active_for_tenant(&b.pool, b.tenant_id)
            .await
            .unwrap()
            .is_empty(),
        "a platform switch in region A must not block region B's tenant"
    );

    a.drop(&admin).await;
    b.drop(&admin).await;
}

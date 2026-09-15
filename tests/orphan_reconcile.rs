//! `orphan_reconcile` integration suite (DESIGN.md §4.4/§10, T-022, T-030),
//! following `tests/partition_lifecycle.rs`/`tests/keystore.rs` conventions:
//! real tenant provisioning, real Vault dev-mode Transit, no mocks.

use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

use messgr::db;
use messgr::encryption;
use messgr::keystore::VaultKeyStore;
use messgr::orphan_reconcile::reconcile::run_for_tenant;
use messgr::profile::Profile;
use messgr::tenant::pool::connect_tenant_pool;
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

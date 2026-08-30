//! Producer registry integration suite (DESIGN.md §4.9, T-005), following
//! `tests/tenancy.rs`'s conventions: real provisioning against the local
//! stack, no mocks.

use sqlx::{Executor, PgPool};
use uuid::Uuid;

use messgr::db;
use messgr::keystore::VaultKeyStore;
use messgr::producer::cert_repo;
use messgr::producer::register::{disable_producer, list_producers, register_producer};
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
    if let Err(err) = control_pool.execute(terminate.as_str()).await {
        eprintln!("cleanup: failed to terminate backends on {database_name}: {err}");
    }

    let drop_db = format!("DROP DATABASE IF EXISTS \"{database_name}\"");
    if let Err(err) = control_pool.execute(drop_db.as_str()).await {
        eprintln!("cleanup: failed to drop database {database_name}: {err}");
    }

    if let Err(err) = sqlx::query("DELETE FROM tenant_schema_version WHERE tenant_id = (SELECT id FROM tenant WHERE slug = $1)")
        .bind(slug)
        .execute(control_pool)
        .await
    {
        eprintln!("cleanup: failed to delete tenant_schema_version for {slug}: {err}");
    }

    if let Err(err) = sqlx::query("DELETE FROM platform_audit WHERE tenant_id = (SELECT id FROM tenant WHERE slug = $1)")
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

async fn provision_test_tenant(
    control_pool: &PgPool,
    control_url: &str,
    vault: &VaultKeyStore,
    slug: &str,
    database_name: &str,
) -> Uuid {
    provision_tenant(
        control_pool,
        control_url,
        slug,
        "eu",
        database_name,
        Profile::Dev,
        "test-actor",
        vault.client(),
    )
    .await
    .expect("provisioning test tenant failed")
    .tenant_id
}

async fn cleanup_cert(control_pool: &PgPool, cert_subject: &str) {
    if let Err(err) = cert_repo::delete_producer_cert(control_pool, cert_subject).await
    {
        eprintln!(
            "cleanup: failed to delete producer_cert row for {cert_subject}: {err}"
        );
    }
}

#[tokio::test]
async fn register_writes_both_the_tenant_row_and_the_control_mapping() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_tenant_producer_reg");
    let db_name = unique_name("test_db_producer_reg");
    let cert_subject = format!("CN={}", unique_name("fraud"));

    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name)
            .await;

    let outcome = register_producer(
        &control_pool,
        &control_url,
        &slug,
        "fraud-alerts",
        &cert_subject,
        "fraud",
        "fraud-oncall@example.com",
        Profile::Dev,
        "test-actor",
    )
    .await
    .expect("registering producer failed");

    assert_eq!(outcome.outcome, "created");

    let cert = cert_repo::find_producer_cert(&control_pool, &cert_subject)
        .await
        .expect("querying producer_cert failed")
        .expect("producer_cert row must exist after registration");
    assert_eq!(cert.tenant_id, tenant_id);
    assert_eq!(cert.producer_id, outcome.producer_id);

    let producers = list_producers(&control_pool, &control_url, &slug, Profile::Dev)
        .await
        .expect("listing producers failed");
    assert_eq!(producers.len(), 1);
    assert_eq!(producers[0].id, outcome.producer_id);
    assert_eq!(producers[0].cert_subject, cert_subject);
    assert!(producers[0].enabled);

    cleanup_cert(&control_pool, &cert_subject).await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn two_tenants_registering_the_same_producer_name_are_isolated() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug_a = unique_name("test_tenant_producer_a");
    let db_a = unique_name("test_db_producer_a");
    let slug_b = unique_name("test_tenant_producer_b");
    let db_b = unique_name("test_db_producer_b");
    let cert_a = format!("CN={}", unique_name("shared-name-a"));
    let cert_b = format!("CN={}", unique_name("shared-name-b"));

    provision_test_tenant(&control_pool, &control_url, &vault, &slug_a, &db_a).await;
    provision_test_tenant(&control_pool, &control_url, &vault, &slug_b, &db_b).await;

    let outcome_a = register_producer(
        &control_pool,
        &control_url,
        &slug_a,
        "shared-name",
        &cert_a,
        "team-a",
        "a@example.com",
        Profile::Dev,
        "test-actor",
    )
    .await
    .expect("registering producer for tenant A failed");

    let outcome_b = register_producer(
        &control_pool,
        &control_url,
        &slug_b,
        "shared-name",
        &cert_b,
        "team-b",
        "b@example.com",
        Profile::Dev,
        "test-actor",
    )
    .await
    .expect("registering producer for tenant B failed");

    assert_ne!(
        outcome_a.producer_id, outcome_b.producer_id,
        "the same producer name in two tenants must get two distinct producer ids"
    );

    let producers_a =
        list_producers(&control_pool, &control_url, &slug_a, Profile::Dev)
            .await
            .expect("listing producers for tenant A failed");
    assert_eq!(
        producers_a.len(),
        1,
        "tenant A's producer list must not include tenant B's producer"
    );
    assert_eq!(producers_a[0].id, outcome_a.producer_id);

    cleanup_cert(&control_pool, &cert_a).await;
    cleanup_cert(&control_pool, &cert_b).await;
    drop_test_tenant(&control_pool, &db_a, &slug_a).await;
    drop_test_tenant(&control_pool, &db_b, &slug_b).await;
}

#[tokio::test]
async fn idempotent_reregistration_writes_a_second_audit_row_and_no_duplicate() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_tenant_producer_idem");
    let db_name = unique_name("test_db_producer_idem");
    let cert_subject = format!("CN={}", unique_name("idempotent"));

    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name)
            .await;

    let first = register_producer(
        &control_pool,
        &control_url,
        &slug,
        "idempotent-producer",
        &cert_subject,
        "team",
        "team@example.com",
        Profile::Dev,
        "test-actor",
    )
    .await
    .expect("first registration failed");
    assert_eq!(first.outcome, "created");

    let second = register_producer(
        &control_pool,
        &control_url,
        &slug,
        "idempotent-producer",
        &cert_subject,
        "team",
        "team@example.com",
        Profile::Dev,
        "test-actor",
    )
    .await
    .expect("idempotent re-registration failed");
    assert_eq!(second.outcome, "idempotent");
    assert_eq!(second.producer_id, first.producer_id);

    let producers = list_producers(&control_pool, &control_url, &slug, Profile::Dev)
        .await
        .expect("listing producers failed");
    assert_eq!(
        producers.len(),
        1,
        "no duplicate producer row must be created"
    );

    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT action, detail->>'outcome' FROM platform_audit \
         WHERE tenant_id = $1 AND action = 'producer.register' ORDER BY at",
    )
    .bind(tenant_id)
    .fetch_all(&control_pool)
    .await
    .expect("querying platform_audit failed");

    assert_eq!(
        rows,
        vec![
            ("producer.register".to_string(), Some("created".to_string())),
            (
                "producer.register".to_string(),
                Some("idempotent".to_string())
            ),
        ]
    );

    cleanup_cert(&control_pool, &cert_subject).await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn conflicting_reregistration_is_rejected_and_audited() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_tenant_producer_conflict");
    let db_name = unique_name("test_db_producer_conflict");
    let cert_subject = format!("CN={}", unique_name("conflict"));
    let other_cert_subject = format!("CN={}", unique_name("conflict-other"));

    let tenant_id =
        provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name)
            .await;

    register_producer(
        &control_pool,
        &control_url,
        &slug,
        "conflict-producer",
        &cert_subject,
        "team",
        "team@example.com",
        Profile::Dev,
        "test-actor",
    )
    .await
    .expect("first registration failed");

    let result = register_producer(
        &control_pool,
        &control_url,
        &slug,
        "conflict-producer",
        &other_cert_subject,
        "team",
        "team@example.com",
        Profile::Dev,
        "test-actor",
    )
    .await;

    assert!(
        result.is_err(),
        "re-registering the same name with a different cert_subject must be rejected"
    );

    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT action, detail->>'outcome' FROM platform_audit \
         WHERE tenant_id = $1 AND action = 'producer.register' AND detail->>'outcome' = 'rejected'",
    )
    .bind(tenant_id)
    .fetch_all(&control_pool)
    .await
    .expect("querying platform_audit failed");

    assert_eq!(
        rows.len(),
        1,
        "the rejected attempt must write exactly one audit row"
    );

    cleanup_cert(&control_pool, &cert_subject).await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn cert_subject_already_bound_to_a_different_tenant_is_rejected() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug_a = unique_name("test_tenant_producer_imp_a");
    let db_a = unique_name("test_db_producer_imp_a");
    let slug_b = unique_name("test_tenant_producer_imp_b");
    let db_b = unique_name("test_db_producer_imp_b");
    let cert_subject = format!("CN={}", unique_name("impersonation"));

    provision_test_tenant(&control_pool, &control_url, &vault, &slug_a, &db_a).await;
    provision_test_tenant(&control_pool, &control_url, &vault, &slug_b, &db_b).await;

    register_producer(
        &control_pool,
        &control_url,
        &slug_a,
        "producer-a",
        &cert_subject,
        "team-a",
        "a@example.com",
        Profile::Dev,
        "test-actor",
    )
    .await
    .expect("registering producer for tenant A failed");

    let result = register_producer(
        &control_pool,
        &control_url,
        &slug_b,
        "producer-b",
        &cert_subject,
        "team-b",
        "b@example.com",
        Profile::Dev,
        "test-actor",
    )
    .await;

    assert!(
        result.is_err(),
        "a cert_subject already bound to a different tenant must be rejected \
         (this is the cross-tenant impersonation case)"
    );

    let producers_b =
        list_producers(&control_pool, &control_url, &slug_b, Profile::Dev)
            .await
            .expect("listing producers for tenant B failed");
    assert!(
        producers_b.is_empty(),
        "the rejected registration must not create a producer row in tenant B"
    );

    cleanup_cert(&control_pool, &cert_subject).await;
    drop_test_tenant(&control_pool, &db_a, &slug_a).await;
    drop_test_tenant(&control_pool, &db_b, &slug_b).await;
}

#[tokio::test]
async fn disable_keeps_the_cert_mapping_and_is_idempotent() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_tenant_producer_disable");
    let db_name = unique_name("test_db_producer_disable");
    let cert_subject = format!("CN={}", unique_name("disable"));

    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    register_producer(
        &control_pool,
        &control_url,
        &slug,
        "disable-producer",
        &cert_subject,
        "team",
        "team@example.com",
        Profile::Dev,
        "test-actor",
    )
    .await
    .expect("registration failed");

    let first_disable = disable_producer(
        &control_pool,
        &control_url,
        &slug,
        "disable-producer",
        Profile::Dev,
        "test-actor",
    )
    .await
    .expect("first disable failed");
    assert_eq!(first_disable.outcome, "disabled");

    let producers = list_producers(&control_pool, &control_url, &slug, Profile::Dev)
        .await
        .expect("listing producers failed");
    assert_eq!(producers.len(), 1);
    assert!(!producers[0].enabled);

    let cert = cert_repo::find_producer_cert(&control_pool, &cert_subject)
        .await
        .expect("querying producer_cert failed");
    assert!(
        cert.is_some(),
        "disable must leave the producer_cert mapping in place (decision 5)"
    );

    let second_disable = disable_producer(
        &control_pool,
        &control_url,
        &slug,
        "disable-producer",
        Profile::Dev,
        "test-actor",
    )
    .await
    .expect("disabling an already-disabled producer must succeed");
    assert_eq!(second_disable.outcome, "idempotent");

    cleanup_cert(&control_pool, &cert_subject).await;
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn register_and_disable_against_an_unknown_tenant_slug_are_rejected_and_audited()
{
    // Review finding T-005/F1: register_producer/disable_producer used to
    // return early on an unknown --tenant-slug before ever reaching the
    // audit() call, so a rejected attempt against a tenant that doesn't
    // exist wrote no platform_audit row at all — contradicting decision 4
    // ("every register/disable attempt ... including the rejected ones").
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");

    let unknown_slug = unique_name("test_tenant_unknown_slug");
    let unique_actor = unique_name("test-actor-unknown-slug");

    let register_result = register_producer(
        &control_pool,
        &control_url,
        &unknown_slug,
        "some-producer",
        "CN=some-producer.internal",
        "team",
        "team@example.com",
        Profile::Dev,
        &unique_actor,
    )
    .await;
    assert!(
        register_result.is_err(),
        "registering against an unknown tenant slug must be rejected"
    );

    let disable_result = disable_producer(
        &control_pool,
        &control_url,
        &unknown_slug,
        "some-producer",
        Profile::Dev,
        &unique_actor,
    )
    .await;
    assert!(
        disable_result.is_err(),
        "disabling against an unknown tenant slug must be rejected"
    );

    let rows: Vec<(String, Option<String>, bool)> = sqlx::query_as(
        "SELECT action, detail->>'outcome', tenant_id IS NULL FROM platform_audit \
         WHERE actor = $1 ORDER BY at",
    )
    .bind(&unique_actor)
    .fetch_all(&control_pool)
    .await
    .expect("querying platform_audit failed");

    assert_eq!(
        rows,
        vec![
            (
                "producer.register".to_string(),
                Some("rejected".to_string()),
                true
            ),
            (
                "producer.disable".to_string(),
                Some("rejected".to_string()),
                true
            ),
        ],
        "both attempts against an unknown tenant slug must write a rejected platform_audit \
         row with no tenant_id, not skip auditing entirely"
    );

    if let Err(err) = sqlx::query("DELETE FROM platform_audit WHERE actor = $1")
        .bind(&unique_actor)
        .execute(&control_pool)
        .await
    {
        eprintln!(
            "cleanup: failed to delete platform_audit rows for actor {unique_actor}: {err}"
        );
    }
}

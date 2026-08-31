//! `template` integration suite (DESIGN.md §4.4, T-010), following
//! `tests/tenant_config.rs`'s conventions: real provisioning against the
//! local stack, no mocks.

use std::collections::HashMap;

use sqlx::PgPool;
use uuid::Uuid;

use messgr::db;
use messgr::keystore::VaultKeyStore;
use messgr::profile::Profile;
use messgr::template::approve::{
    ApproveError, approve_template, list_template_versions, render_preview,
    show_template,
};
use messgr::template::model::channel;
use messgr::template::render::RenderError;
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

fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
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

#[tokio::test]
async fn approving_and_showing_round_trips_every_field() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_template_roundtrip");
    let db_name = unique_name("test_db_template_roundtrip");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    let outcome = approve_template(
        &control_pool,
        &control_url,
        &slug,
        "balance-alert",
        1,
        channel::SMS,
        "en-GB",
        "Hi {{name}}, your balance is {{balance}}.",
        Profile::Dev,
        "test-actor",
    )
    .await
    .expect("approving template failed");
    assert_eq!(outcome.outcome, "created");

    let loaded = show_template(
        &control_pool,
        &control_url,
        &slug,
        "balance-alert",
        1,
        "en-GB",
        Profile::Dev,
    )
    .await
    .expect("showing template failed")
    .expect("template row must exist after approve_template");

    assert_eq!(loaded.template_id, "balance-alert");
    assert_eq!(loaded.version, 1);
    assert_eq!(loaded.channel, channel::SMS);
    assert_eq!(loaded.locale, "en-GB");
    assert_eq!(loaded.body, "Hi {{name}}, your balance is {{balance}}.");
    assert_eq!(loaded.approved_by, "test-actor");

    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn re_approving_the_same_version_locale_is_rejected_and_audited() {
    // Guards against templates silently becoming mutable: a repeat approval
    // for an already-approved (template_id, version, locale) must be
    // rejected, never idempotent or upserted (T-010 decision 5).
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_template_reapprove");
    let db_name = unique_name("test_db_template_reapprove");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;
    let unique_actor = unique_name("test-actor-reapprove");

    let first = approve_template(
        &control_pool,
        &control_url,
        &slug,
        "balance-alert",
        1,
        channel::SMS,
        "en-GB",
        "Hi {{name}}.",
        Profile::Dev,
        &unique_actor,
    )
    .await
    .expect("first approve failed");
    assert_eq!(first.outcome, "created");

    let second = approve_template(
        &control_pool,
        &control_url,
        &slug,
        "balance-alert",
        1,
        channel::SMS,
        "en-GB",
        "Hi {{name}}, a different body.",
        Profile::Dev,
        &unique_actor,
    )
    .await;
    assert!(
        second.is_err(),
        "re-approving the same (template_id, version, locale) must be rejected"
    );

    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT action, detail->>'outcome' FROM platform_audit \
         WHERE actor = $1 ORDER BY at",
    )
    .bind(&unique_actor)
    .fetch_all(&control_pool)
    .await
    .expect("querying platform_audit failed");

    assert_eq!(
        rows,
        vec![
            ("template.approve".to_string(), Some("created".to_string())),
            ("template.approve".to_string(), Some("rejected".to_string())),
        ],
        "both the created and the rejected re-approval must write platform_audit rows"
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
    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn list_versions_returns_every_version_and_locale_ordered() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_template_list");
    let db_name = unique_name("test_db_template_list");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    for (version, locale) in [(1, "en-GB"), (2, "en-GB"), (2, "fr-FR")] {
        approve_template(
            &control_pool,
            &control_url,
            &slug,
            "balance-alert",
            version,
            channel::SMS,
            locale,
            "Hi {{name}}.",
            Profile::Dev,
            "test-actor",
        )
        .await
        .expect("approving template failed");
    }

    let versions = list_template_versions(
        &control_pool,
        &control_url,
        &slug,
        "balance-alert",
        Profile::Dev,
    )
    .await
    .expect("listing template versions failed");

    let observed: Vec<(i32, String)> = versions
        .into_iter()
        .map(|template| (template.version, template.locale))
        .collect();
    assert_eq!(
        observed,
        vec![
            (1, "en-GB".to_string()),
            (2, "en-GB".to_string()),
            (2, "fr-FR".to_string()),
        ]
    );

    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn render_preview_substitutes_every_supplied_variable() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_template_render");
    let db_name = unique_name("test_db_template_render");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    approve_template(
        &control_pool,
        &control_url,
        &slug,
        "balance-alert",
        1,
        channel::SMS,
        "en-GB",
        "Hi {{ name }}, your balance is {{balance}}.",
        Profile::Dev,
        "test-actor",
    )
    .await
    .expect("approving template failed");

    let rendered = render_preview(
        &control_pool,
        &control_url,
        &slug,
        "balance-alert",
        1,
        "en-GB",
        &vars(&[("name", "Jordan"), ("balance", "£120.00")]),
        Profile::Dev,
    )
    .await
    .expect("rendering template failed");

    assert_eq!(rendered, "Hi Jordan, your balance is £120.00.");

    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn render_preview_with_a_missing_variable_fails_without_a_partial_result() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let vault = vault_keystore();

    let slug = unique_name("test_template_missing_var");
    let db_name = unique_name("test_db_template_missing_var");
    provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name).await;

    approve_template(
        &control_pool,
        &control_url,
        &slug,
        "balance-alert",
        1,
        channel::SMS,
        "en-GB",
        "Hi {{name}}, your balance is {{balance}}.",
        Profile::Dev,
        "test-actor",
    )
    .await
    .expect("approving template failed");

    let result = render_preview(
        &control_pool,
        &control_url,
        &slug,
        "balance-alert",
        1,
        "en-GB",
        &vars(&[("name", "Jordan")]),
        Profile::Dev,
    )
    .await;

    match result {
        Err(ApproveError::Render(RenderError::MissingVariable(key))) => {
            assert_eq!(key, "balance");
        }
        other => panic!(
            "expected ApproveError::Render(RenderError::MissingVariable(\"balance\")), got {other:?}"
        ),
    }

    drop_test_tenant(&control_pool, &db_name, &slug).await;
}

#[tokio::test]
async fn approve_template_against_an_unknown_tenant_slug_is_rejected_and_audited() {
    // Guards against the T-005/F1 mistake: an unknown tenant slug must not
    // skip the platform_audit write just because it returns early.
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");

    let unknown_slug = unique_name("test_template_unknown_slug");
    let unique_actor = unique_name("test-actor-unknown-slug");

    let result = approve_template(
        &control_pool,
        &control_url,
        &unknown_slug,
        "balance-alert",
        1,
        channel::SMS,
        "en-GB",
        "Hi {{name}}.",
        Profile::Dev,
        &unique_actor,
    )
    .await;
    assert!(
        result.is_err(),
        "approving a template against an unknown tenant slug must be rejected"
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
        vec![(
            "template.approve".to_string(),
            Some("rejected".to_string()),
            true
        )],
        "a rejected attempt against an unknown tenant slug must write a rejected \
         platform_audit row with no tenant_id, not skip auditing entirely"
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

//! Proves the Vault-side isolation T-004 introduces: a tenant's own AppRole
//! credentials can operate on its own Transit mount and are rejected — by
//! Vault's ACL, not merely by Transit's own cross-mount key mismatch
//! (already covered by tests/keystore.rs) — on any other tenant's mount.

use uuid::Uuid;
use vaultrs::api::transit::requests::DataKeyType;
use vaultrs::client::VaultClient;
use vaultrs::transit::generate;

use messgr::db;
use messgr::keystore::{KeyStore, VaultKeyStore};
use messgr::profile::Profile;
use messgr::tenant::provision::provision_tenant;
use messgr::tenant::vault as tenant_vault;
use sqlx::{Executor, PgPool};

fn control_database_url() -> String {
    dotenvy::dotenv().ok();
    std::env::var("CONTROL_DATABASE_URL")
        .expect("CONTROL_DATABASE_URL must be set for tests")
}

fn unique_name(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

async fn drop_test_tenant(
    control_pool: &PgPool,
    admin: &VaultClient,
    database_name: &str,
    slug: &str,
) {
    // Best-effort teardown, same shape as tests/tenancy.rs's helper, plus the
    // Vault-side resources this ticket adds — so repeated CI runs don't
    // accumulate tenant mounts/roles/policies in the shared dev-mode Vault.
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

    if let Err(err) = vaultrs::auth::approle::role::delete(admin, "approle", slug).await
    {
        eprintln!("cleanup: failed to delete AppRole for {slug}: {err}");
    }
    if let Err(err) =
        vaultrs::sys::policy::delete(admin, &format!("tenant-{slug}-transit")).await
    {
        eprintln!("cleanup: failed to delete policy for {slug}: {err}");
    }
    if let Err(err) =
        vaultrs::sys::mount::disable(admin, &format!("transit/{slug}")).await
    {
        eprintln!("cleanup: failed to disable Transit mount for {slug}: {err}");
    }
}

#[tokio::test]
async fn tenant_a_vault_credentials_cannot_read_tenant_bs_dek() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let admin = VaultKeyStore::connect(Profile::Dev)
        .expect("connecting to dev-mode Vault failed");

    let slug_a = unique_name("test_tenant_vault_a");
    let db_a = unique_name("test_db_vault_a");
    let slug_b = unique_name("test_tenant_vault_b");
    let db_b = unique_name("test_db_vault_b");

    let outcome_a = provision_tenant(
        &control_pool,
        &control_url,
        &slug_a,
        "eu",
        &db_a,
        "test-actor",
        admin.client(),
    )
    .await
    .expect("provisioning tenant A failed");
    // Tenant B's own outcome (RoleID/SecretID) isn't needed here — only its
    // mount needs to exist for tenant A's credentials to be tried against it.
    provision_tenant(
        &control_pool,
        &control_url,
        &slug_b,
        "eu",
        &db_b,
        "test-actor",
        admin.client(),
    )
    .await
    .expect("provisioning tenant B failed");

    let wrapped_a = outcome_a
        .vault_wrapped_secret_id
        .expect("a fresh provision must mint a SecretID");

    let scoped_a = VaultKeyStore::login_as_tenant(
        &outcome_a.vault_role_id,
        &wrapped_a,
        Profile::Dev,
    )
    .await
    .expect("AppRole login must succeed with a freshly minted RoleID/wrapped SecretID");

    // Tenant A, on its own mount: must succeed.
    let own_mount = format!("transit/{slug_a}");
    generate::data_key(
        scoped_a.client(),
        &own_mount,
        "messgr-dek",
        DataKeyType::Plaintext,
        None,
    )
    .await
    .expect("tenant A must be able to create a DEK on its own mount");

    // Tenant A's credentials, on tenant B's mount: must be rejected by
    // Vault's ACL (permission denied), not merely fail for some other reason.
    let other_mount = format!("transit/{slug_b}");
    let result = generate::data_key(
        scoped_a.client(),
        &other_mount,
        "messgr-dek",
        DataKeyType::Plaintext,
        None,
    )
    .await;
    assert!(
        result.is_err(),
        "tenant A's Vault credentials must not be able to operate on tenant B's mount"
    );

    // Same mutation standard as T-003/F1: prove it's really the tenant-scoped
    // policy doing the rejecting, not an accident of the fixture, by
    // confirming the *admin* client (unscoped) can reach the same path fine.
    generate::data_key(
        admin.client(),
        &other_mount,
        "messgr-dek",
        DataKeyType::Plaintext,
        None,
    )
    .await
    .expect("the admin client must still be able to reach tenant B's mount directly");

    drop_test_tenant(&control_pool, admin.client(), &db_a, &slug_a).await;
    drop_test_tenant(&control_pool, admin.client(), &db_b, &slug_b).await;
}

#[tokio::test]
async fn tenant_a_vault_credentials_can_read_its_own_provider_secret_but_not_tenant_bs()
{
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let admin = VaultKeyStore::connect(Profile::Dev)
        .expect("connecting to dev-mode Vault failed");

    let slug_a = unique_name("test_tenant_kv_a");
    let db_a = unique_name("test_db_kv_a");
    let slug_b = unique_name("test_tenant_kv_b");
    let db_b = unique_name("test_db_kv_b");

    let outcome_a = provision_tenant(
        &control_pool,
        &control_url,
        &slug_a,
        "eu",
        &db_a,
        "test-actor",
        admin.client(),
    )
    .await
    .expect("provisioning tenant A failed");
    // Tenant B's own outcome (RoleID/SecretID) isn't needed here — only its
    // KV prefix needs a secret in it for tenant A's credentials to be tried
    // against it.
    provision_tenant(
        &control_pool,
        &control_url,
        &slug_b,
        "eu",
        &db_b,
        "test-actor",
        admin.client(),
    )
    .await
    .expect("provisioning tenant B failed");

    vaultrs::kv2::set(
        admin.client(),
        "secret",
        &format!("{slug_a}/sms"),
        &serde_json::json!({"api_key": "tenant-a-key"}),
    )
    .await
    .expect("admin must be able to write tenant A's provider secret");
    vaultrs::kv2::set(
        admin.client(),
        "secret",
        &format!("{slug_b}/sms"),
        &serde_json::json!({"api_key": "tenant-b-key"}),
    )
    .await
    .expect("admin must be able to write tenant B's provider secret");

    let wrapped_a = outcome_a
        .vault_wrapped_secret_id
        .expect("a fresh provision must mint a SecretID");

    let scoped_a = VaultKeyStore::login_as_tenant(
        &outcome_a.vault_role_id,
        &wrapped_a,
        Profile::Dev,
    )
    .await
    .expect("AppRole login must succeed with a freshly minted RoleID/wrapped SecretID");

    // Tenant A, on its own KV prefix: must succeed.
    let api_key = scoped_a
        .read_provider_credential("secret", &format!("{slug_a}/sms"))
        .await
        .expect("tenant A must be able to read its own provider secret");
    assert_eq!(api_key, "tenant-a-key");

    // Tenant A's credentials, on tenant B's KV prefix: must be rejected by
    // Vault's ACL (permission denied), not merely fail for some other reason.
    let result = scoped_a
        .read_provider_credential("secret", &format!("{slug_b}/sms"))
        .await;
    assert!(
        result.is_err(),
        "tenant A's Vault credentials must not be able to read tenant B's provider secret"
    );

    // Same mutation standard as tenant_a_vault_credentials_cannot_read_tenant_bs_dek:
    // prove it's really the tenant-scoped policy doing the rejecting, not an
    // accident of the fixture, by confirming the *admin* client can still
    // reach the same path fine.
    admin
        .read_provider_credential("secret", &format!("{slug_b}/sms"))
        .await
        .expect("the admin client must still be able to read tenant B's provider secret directly");

    drop_test_tenant(&control_pool, admin.client(), &db_a, &slug_a).await;
    drop_test_tenant(&control_pool, admin.client(), &db_b, &slug_b).await;
}

#[tokio::test]
async fn idempotent_reprovision_does_not_mint_a_second_secret_id() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let admin = VaultKeyStore::connect(Profile::Dev)
        .expect("connecting to dev-mode Vault failed");

    let slug = unique_name("test_tenant_vault_idempotent");
    let db_name = unique_name("test_db_vault_idempotent");

    let first = provision_tenant(
        &control_pool,
        &control_url,
        &slug,
        "eu",
        &db_name,
        "test-actor",
        admin.client(),
    )
    .await
    .expect("first provisioning failed");
    assert!(
        first.vault_wrapped_secret_id.is_some(),
        "a fresh provision must mint a SecretID"
    );

    let second = provision_tenant(
        &control_pool,
        &control_url,
        &slug,
        "eu",
        &db_name,
        "test-actor",
        admin.client(),
    )
    .await
    .expect("idempotent re-provisioning failed");
    assert!(
        second.vault_wrapped_secret_id.is_none(),
        "an idempotent re-provision must not mint a second SecretID (decision 5)"
    );
    assert_eq!(
        first.vault_role_id, second.vault_role_id,
        "re-provisioning the same tenant must return the same RoleID"
    );

    drop_test_tenant(&control_pool, admin.client(), &db_name, &slug).await;
}

#[tokio::test]
async fn provisioning_creates_a_mount_scoped_to_exactly_this_tenant() {
    // Narrower unit-style check on `tenant::vault::provision_vault` directly
    // (not through `provision_tenant`, so it needs no Postgres tenant row):
    // the mount this ticket creates exists, and is scoped by slug.
    let admin = VaultKeyStore::connect(Profile::Dev)
        .expect("connecting to dev-mode Vault failed");
    let slug = unique_name("test_tenant_vault_mount_only");

    let outcome = tenant_vault::provision_vault(admin.client(), &slug, true)
        .await
        .expect("provision_vault failed");
    assert!(!outcome.role_id.is_empty(), "role_id must not be empty");
    assert!(
        outcome.wrapped_secret_id.is_some(),
        "mint_secret_id=true must produce a wrapped SecretID"
    );

    let mounts = vaultrs::sys::mount::list(admin.client())
        .await
        .expect("listing mounts failed");
    assert!(
        mounts.contains_key(&format!("transit/{slug}/")),
        "provisioning must create a mount at transit/<slug>/"
    );

    // No Postgres tenant row exists for this test (it calls `provision_vault`
    // directly, not `provision_tenant`), so only the Vault-side resources
    // need cleanup, not the full `drop_test_tenant` helper.
    let _ =
        vaultrs::auth::approle::role::delete(admin.client(), "approle", &slug).await;
    let _ =
        vaultrs::sys::policy::delete(admin.client(), &format!("tenant-{slug}-transit"))
            .await;
    let _ =
        vaultrs::sys::mount::disable(admin.client(), &format!("transit/{slug}")).await;
}

#[tokio::test]
async fn login_as_tenant_authenticates_and_can_create_a_dek_on_its_own_mount() {
    // Proves T-008's promoted production AppRole-login path actually
    // authenticates end to end — not just that the isolation assertions
    // above still pass with a differently-constructed scoped client.
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let admin = VaultKeyStore::connect(Profile::Dev)
        .expect("connecting to dev-mode Vault failed");

    let slug = unique_name("test_tenant_approle_login");
    let db_name = unique_name("test_db_approle_login");

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
    let wrapped = outcome
        .vault_wrapped_secret_id
        .expect("a fresh provision must mint a SecretID");

    let scoped = VaultKeyStore::login_as_tenant(
        &outcome.vault_role_id,
        &wrapped,
        Profile::Dev,
    )
    .await
    .expect("AppRole login must succeed with a freshly minted RoleID/wrapped SecretID");

    let dek = scoped
        .create_dek(&format!("transit/{slug}"))
        .await
        .expect("a tenant-scoped login must be able to create a DEK on its own mount");
    assert_eq!(dek.plaintext.len(), 32);

    drop_test_tenant(&control_pool, admin.client(), &db_name, &slug).await;
}

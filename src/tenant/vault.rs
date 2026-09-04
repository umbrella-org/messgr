//! Per-tenant Vault Transit mount, key, ACL policy, and AppRole provisioning
//! (DESIGN.md §7.6, §11.4). Fills the seam `keystore::KeyStore` (T-003)
//! deliberately left open: creating the substrate `KeyStore` talks to.
//! Assumes the `approle` auth method is already enabled cluster-wide (a
//! one-time Vault bootstrap — `just vault-dev-init` / the CI bootstrap step
//! enable it once; see the ticket's decision 3). Never authenticates *as* a
//! tenant — that is T-009's job.

use std::collections::HashMap;

use vaultrs::api::ResponseWrapper;
use vaultrs::api::auth::approle::requests::{
    GenerateNewSecretIDRequest, SetAppRoleRequest,
};
use vaultrs::api::sys::responses::MountResponse;
use vaultrs::auth::approle::role as approle_role;
use vaultrs::client::VaultClient;
use vaultrs::error::ClientError;
use vaultrs::sys::{mount, policy};
use vaultrs::transit::key as transit_key;

use crate::keystore::{KEY_NAME, KeyStoreError};

const APPROLE_MOUNT: &str = "approle";

/// The result of provisioning (or idempotently re-confirming) one tenant's
/// Vault identity.
pub struct VaultProvisionOutcome {
    /// Public AppRole identifier — safe to persist (`tenant.vault_role_id`).
    pub role_id: String,
    /// A response-wrapped SecretID token (10-minute default TTL, single-use
    /// unwrap), present only when `mint_secret_id` was true. Print once for
    /// out-of-band delivery; never persist.
    pub wrapped_secret_id: Option<String>,
}

/// Creates (or idempotently re-confirms) `tenant_slug`'s Transit mount, key,
/// ACL policy, and AppRole. Mints a new response-wrapped SecretID only when
/// `mint_secret_id` is true (decision 5) — pass `false` on an idempotent
/// re-provision of an already-active tenant.
pub async fn provision_vault(
    client: &VaultClient,
    tenant_slug: &str,
    mint_secret_id: bool,
) -> Result<VaultProvisionOutcome, KeyStoreError> {
    let mount_path = format!("transit/{tenant_slug}");

    ensure_transit_mount(client, &mount_path).await?;
    // Idempotent: Vault's documented behaviour is that creating a key that
    // already exists (same type) is a no-op, not an error.
    transit_key::create(client, &mount_path, KEY_NAME, None).await?;

    let policy_name = format!("tenant-{tenant_slug}-transit");
    let policy_hcl = policy_hcl_for(&mount_path, tenant_slug);
    // `policy::set` is an upsert — no idempotency concern.
    policy::set(client, &policy_name, &policy_hcl).await?;

    let mut role_opts = SetAppRoleRequest::builder();
    role_opts.token_policies(vec![policy_name]);
    // `approle_role::set` is create-or-update — no idempotency concern.
    approle_role::set(client, APPROLE_MOUNT, tenant_slug, Some(&mut role_opts)).await?;

    let role_id = approle_role::read_id(client, APPROLE_MOUNT, tenant_slug)
        .await?
        .role_id;

    let wrapped_secret_id = if mint_secret_id {
        let endpoint = GenerateNewSecretIDRequest::builder()
            .mount(APPROLE_MOUNT)
            .role_name(tenant_slug)
            .build()
            .expect("static builder inputs always build");
        let wrapped = endpoint.wrap(client).await.map_err(KeyStoreError::from)?;
        Some(wrapped.info.token)
    } else {
        None
    };

    Ok(VaultProvisionOutcome {
        role_id,
        wrapped_secret_id,
    })
}

/// The ACL policy granting exactly the two paths `KeyStore`'s two methods
/// touch under `mount_path` — tighter than "the whole mount" (which would
/// also permit key rotate/export/delete) — plus `read` on the tenant's own
/// provider-credential KV prefix (DESIGN.md §13, T-023 decision 4). Still
/// satisfies §7.6's "an AppRole [bound] to exactly one mount": a subset of
/// paths across the tenant's own Transit mount and its own KV prefix is
/// still exactly-scoped to that one tenant. Key name is always the fixed
/// constant, so there is no wildcard on Transit; the KV wildcard is narrow
/// (per-tenant prefix) because provider paths are per-channel and open-ended.
fn policy_hcl_for(mount_path: &str, tenant_slug: &str) -> String {
    format!(
        "path \"{mount_path}/datakey/plaintext/{KEY_NAME}\" {{\n  capabilities = [\"create\", \"update\"]\n}}\n\
         path \"{mount_path}/decrypt/{KEY_NAME}\" {{\n  capabilities = [\"create\", \"update\"]\n}}\n\
         path \"secret/data/{tenant_slug}/*\" {{\n  capabilities = [\"read\"]\n}}\n"
    )
}

/// Enables a Transit secrets engine at `mount_path` unless one is already
/// mounted there. Checked by listing existing mounts first (Vault's own
/// `sys/mounts` list, keyed by path with a trailing slash Vault always
/// appends) rather than by pattern-matching Vault's error text on a second
/// `enable` call — more robust across Vault versions.
async fn ensure_transit_mount(
    client: &VaultClient,
    mount_path: &str,
) -> Result<(), ClientError> {
    let existing: HashMap<String, MountResponse> = mount::list(client).await?;
    let normalized = format!("{mount_path}/");
    if !existing.contains_key(&normalized) {
        mount::enable(client, mount_path, "transit", None).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_hcl_names_exactly_the_two_keystore_paths_and_the_kv_read_path() {
        // Pure-function regression guard for decision 4 (least privilege): if
        // this ever grows a third Transit path, a broader capability, or a
        // KV path outside the tenant's own prefix, this test should be the
        // thing that has to change, not a live-Vault surprise.
        let policy_hcl = policy_hcl_for("transit/acme", "acme");
        assert!(policy_hcl.contains("transit/acme/datakey/plaintext/messgr-dek"));
        assert!(policy_hcl.contains("transit/acme/decrypt/messgr-dek"));
        assert!(policy_hcl.contains(
            "path \"secret/data/acme/*\" {\n  capabilities = [\"read\"]\n}\n"
        ));
        assert!(!policy_hcl.contains("rotate"));
        assert!(!policy_hcl.contains("export"));
        assert!(!policy_hcl.contains("delete"));
    }
}

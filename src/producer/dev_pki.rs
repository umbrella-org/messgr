//! Dev-only internal PKI for exercising mTLS resolution (`resolve.rs`,
//! T-006) without a real CA. DESIGN.md describes production as using
//! "Vault ... internal PKI" (build order table, row 5) with no engine
//! detail, so dev reuses Vault's own PKI secrets engine rather than
//! inventing a second, divergent internal-CA mechanism — the same Vault
//! already running for Transit (T-003) and already the project's only Vault
//! dependency.
//!
//! Gated behind the same refuse-to-run-outside-`profile=dev` pattern as
//! `MockProvider` (§11.1) and dev-mode Vault (`keystore::assert_tls_outside_dev`,
//! §7.6): a `dev-pki` operation reachable against a real Vault would let an
//! operator mint a trusted producer client certificate outside the
//! registration/audit path entirely.

use std::collections::HashMap;

use vaultrs::api::pki::requests::{
    GenerateCertificateRequest, GenerateRootRequest, SetRoleRequest,
};
use vaultrs::api::pki::responses::GenerateCertificateResponse;
use vaultrs::api::sys::requests::{EnableEngineDataConfigBuilder, EnableEngineRequest};
use vaultrs::api::sys::responses::MountResponse;
use vaultrs::client::VaultClient;
use vaultrs::error::ClientError;
use vaultrs::pki::{cert, issuer, role};
use vaultrs::sys::mount;

use crate::keystore::KeyStoreError;
use crate::profile::Profile;

const PKI_MOUNT: &str = "pki";
const DEV_ROLE: &str = "producer-dev";
/// 10 years. The mount's own `max_lease_ttl` defaults to the system default
/// (32 days) and caps *both* the root CA's and every leaf cert's requested
/// TTL — with the root capped to the same ceiling, issuing a leaf even
/// seconds later can ask for a TTL that would outlive the root itself.
/// Raising the mount ceiling once, at bootstrap, is what makes the root
/// CA's own long TTL (set at generation, below) actually take effect.
const PKI_MAX_LEASE_TTL: &str = "87600h";

/// Serializes the has-issuer-check + generate-root section of `bootstrap`.
/// Unlike `ensure_pki_mount`'s race (caught via Vault's "already in use"
/// error), Vault has no such rejection for a second root CA — it mints one
/// on every `generate` call, no questions asked. Concurrent `bootstrap`
/// callers (e.g. this crate's own test binary, where every `#[tokio::test]`
/// shares one dev Vault) would otherwise each pass the check before either
/// finishes generating, minting two roots.
static ROOT_CA_BOOTSTRAP_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Panics outside `profile = dev` — see the module doc comment. Kept as a
/// free function (not inlined into every call site) so both `bootstrap` and
/// `issue_cert` apply it identically and neither can be added later without
/// it.
fn assert_dev_profile(profile: Profile) {
    if !profile.is_dev() {
        panic!(
            "dev PKI operations are refused outside profile=dev (got {profile:?}) — minting \
             client certificates ad hoc must never be reachable against a real Vault"
        );
    }
}

/// Idempotently ensures the dev `pki` mount, a root CA, and a permissive
/// `producer-dev` role all exist. Safe to call repeatedly (T-006 decision 7):
/// re-running never mints a second root CA or duplicates the mount.
pub async fn bootstrap(
    client: &VaultClient,
    profile: Profile,
) -> Result<(), KeyStoreError> {
    assert_dev_profile(profile);

    ensure_pki_mount(client).await?;

    {
        let _guard = ROOT_CA_BOOTSTRAP_LOCK.lock().await;
        if !has_issuer(client).await? {
            let mut opts = GenerateRootRequest::builder();
            opts.common_name("messgr dev root").ttl(PKI_MAX_LEASE_TTL);
            cert::ca::generate(client, PKI_MOUNT, "internal", Some(&mut opts)).await?;
        }
    }

    // `cert_subject` values are opaque identifiers (e.g. `CN=fraud-alerts.internal`),
    // not real DNS domains, so the role must not enforce hostname-shaped
    // names (decision 7).
    let mut role_opts = SetRoleRequest::builder();
    role_opts.allow_any_name(true).enforce_hostnames(false);
    role::set(client, PKI_MOUNT, DEV_ROLE, Some(&mut role_opts)).await?;

    Ok(())
}

/// Issues a leaf certificate for `common_name` from the dev `producer-dev`
/// role. Callers are expected to have called `bootstrap` first.
pub async fn issue_cert(
    client: &VaultClient,
    profile: Profile,
    common_name: &str,
) -> Result<GenerateCertificateResponse, KeyStoreError> {
    assert_dev_profile(profile);

    let mut opts = GenerateCertificateRequest::builder();
    opts.common_name(common_name);
    Ok(cert::generate(client, PKI_MOUNT, DEV_ROLE, Some(&mut opts)).await?)
}

/// Enables a PKI secrets engine at `PKI_MOUNT` unless one is already
/// mounted there — same list-then-enable pattern as
/// `tenant::vault::ensure_transit_mount`.
async fn ensure_pki_mount(client: &VaultClient) -> Result<(), ClientError> {
    let existing: HashMap<String, MountResponse> = mount::list(client).await?;
    let normalized = format!("{PKI_MOUNT}/");
    if existing.contains_key(&normalized) {
        return Ok(());
    }

    let config = EnableEngineDataConfigBuilder::default()
        .max_lease_ttl(PKI_MAX_LEASE_TTL)
        .build()
        .expect("static builder input always builds");
    let mut opts = EnableEngineRequest::builder();
    opts.config(config);

    match mount::enable(client, PKI_MOUNT, "pki", Some(&mut opts)).await {
        Ok(()) => Ok(()),
        // Unlike ensure_transit_mount's per-tenant mount paths, "pki" is one
        // cluster-wide name every caller of `bootstrap` shares, so the
        // list-then-enable check above is a real TOCTOU race under
        // concurrent bootstrap calls (e.g. parallel test runs). Losing that
        // race — the mount now exists because another caller just created
        // it — is success, not a failure.
        Err(ClientError::APIError { code: 400, errors })
            if errors.iter().any(|err| err.contains("already in use")) =>
        {
            Ok(())
        }
        Err(err) => Err(err),
    }
}

/// Whether the mount already has a root CA. Vault returns a 404 `APIError`
/// for `LIST issuers` on a mount with none yet, rather than an empty list —
/// that response is exactly "no issuer", not a real failure.
async fn has_issuer(client: &VaultClient) -> Result<bool, ClientError> {
    match issuer::list(client, PKI_MOUNT).await {
        Ok(response) => Ok(!response.keys.is_empty()),
        Err(ClientError::APIError { code: 404, .. }) => Ok(false),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_panics_outside_dev() {
        let result =
            std::panic::catch_unwind(|| assert_dev_profile(Profile::Production));
        assert!(
            result.is_err(),
            "dev PKI operations must refuse to run outside profile=dev"
        );
    }

    #[test]
    fn guard_allows_dev() {
        assert_dev_profile(Profile::Dev); // must not panic
    }
}

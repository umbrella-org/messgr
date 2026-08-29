//! Per-customer data-encryption keys via Vault Transit (DESIGN.md §7.6).
//!
//! `KeyStore` is deliberately narrow: two data-plane operations against a
//! mount that is assumed to already exist. Per-tenant mount/AppRole creation
//! is T-004's job; the bounded zeroizing LRU cache and DEK pre-provisioning
//! batch are T-009's. This ticket proves the Transit round-trip the rest of
//! the encryption story is built on.

use async_trait::async_trait;
use base64::Engine as _;
use url::Url;
use vaultrs::api::transit::requests::DataKeyType;
use vaultrs::client::{VaultClient, VaultClientSettings, VaultClientSettingsBuilder};
use vaultrs::transit::{data, generate};
use zeroize::Zeroizing;

use crate::profile::Profile;

/// The key name used inside every tenant's own Transit mount —
/// `transit/<tenant_slug>` (DESIGN.md §7.6). Mount varies per tenant; this
/// never does. `pub(crate)` so `tenant::vault` (T-004) can create a key of
/// this exact name inside the mount it provisions, without a second module
/// defining the same string (drift hazard).
pub(crate) const KEY_NAME: &str = "messgr-dek";

/// One data-encryption key. `plaintext` is zeroized on drop; `wrapped` is the
/// opaque Vault ciphertext to persist as `customer_dek.wrapped_dek` (a later
/// ticket) and hand back to `unwrap_dek`. Never log or persist `plaintext`.
pub struct Dek {
    pub plaintext: Zeroizing<Vec<u8>>,
    pub wrapped: String,
}

#[derive(Debug)]
pub struct KeyStoreError(vaultrs::error::ClientError);

impl std::fmt::Display for KeyStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "vault key store error: {}", self.0)
    }
}

impl std::error::Error for KeyStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl From<vaultrs::error::ClientError> for KeyStoreError {
    fn from(err: vaultrs::error::ClientError) -> Self {
        Self(err)
    }
}

/// A source of per-customer data-encryption keys, backed by Vault Transit.
/// `mount` is the tenant's own Transit mount (`tenant.vault_mount`, §4.11).
#[async_trait]
pub trait KeyStore: Send + Sync {
    async fn create_dek(&self, mount: &str) -> Result<Dek, KeyStoreError>;
    async fn unwrap_dek(
        &self,
        mount: &str,
        wrapped: &str,
    ) -> Result<Zeroizing<Vec<u8>>, KeyStoreError>;
}

pub struct VaultKeyStore {
    client: VaultClient,
}

/// Panics if `profile` is not `Dev` and `address` is not TLS-protected.
/// A dev-mode Vault always serves plain HTTP; a real deployment always uses
/// TLS (§7.6's AppRole/TLS posture) — this is the one signal actually
/// available, since Vault's own API exposes no "this is dev mode" flag to
/// query (checked against `ReadHealthResponse` and `sys::status`). Kept as
/// a free function, not a `VaultKeyStore` method, so it is unit-testable
/// with a hand-built `Url` — no environment mutation, no network call.
fn assert_tls_outside_dev(address: &Url, profile: Profile) {
    if !profile.is_dev() && address.scheme() != "https" {
        panic!(
            "VAULT_ADDR {:?} is not TLS-protected outside profile=dev — refusing to start \
             (a dev-mode Vault reachable in production is a full encryption bypass)",
            address.as_str()
        );
    }
}

impl VaultKeyStore {
    /// Connects using `VAULT_ADDR`/`VAULT_TOKEN` from the environment
    /// (`vaultrs`'s own default — see `VaultClientSettingsBuilder`).
    pub fn connect(profile: Profile) -> Result<Self, KeyStoreError> {
        Ok(Self {
            client: connect_client(profile)?,
        })
    }

    /// The underlying Vault client, for callers that need admin-level
    /// operations `KeyStore`'s own two methods don't cover (T-004's
    /// mount/key/policy/role provisioning). Same guarded, env-based client
    /// either way — see `connect_client`.
    pub fn client(&self) -> &VaultClient {
        &self.client
    }
}

/// Builds a Vault client from `VAULT_ADDR`/`VAULT_TOKEN` (`vaultrs`'s own
/// env defaults), applying the non-dev TLS guard. Shared by
/// `VaultKeyStore::connect` (data-plane, tenant-scoped calls) and
/// `tenant::vault`'s admin provisioning path (T-004) — one way this
/// codebase turns environment variables into a Vault client, not two.
pub(crate) fn connect_client(profile: Profile) -> Result<VaultClient, KeyStoreError> {
    dotenvy::dotenv().ok();

    let settings: VaultClientSettings = VaultClientSettingsBuilder::default()
        .build()
        .unwrap_or_else(|err| panic!("failed to build Vault client settings: {err}"));

    assert_tls_outside_dev(&settings.address, profile);

    Ok(VaultClient::new(settings)?)
}

#[async_trait]
impl KeyStore for VaultKeyStore {
    async fn create_dek(&self, mount: &str) -> Result<Dek, KeyStoreError> {
        let response = generate::data_key(
            &self.client,
            mount,
            KEY_NAME,
            DataKeyType::Plaintext,
            None,
        )
        .await?;
        let plaintext_b64 = response
            .plaintext
            .expect("DataKeyType::Plaintext must return plaintext");
        let plaintext = base64::engine::general_purpose::STANDARD
            .decode(plaintext_b64)
            .expect("vault returned invalid base64 plaintext");
        Ok(Dek {
            plaintext: Zeroizing::new(plaintext),
            wrapped: response.ciphertext,
        })
    }

    async fn unwrap_dek(
        &self,
        mount: &str,
        wrapped: &str,
    ) -> Result<Zeroizing<Vec<u8>>, KeyStoreError> {
        let response =
            data::decrypt(&self.client, mount, KEY_NAME, wrapped, None).await?;
        let plaintext = base64::engine::general_purpose::STANDARD
            .decode(response.plaintext)
            .expect("vault returned invalid base64 plaintext");
        Ok(Zeroizing::new(plaintext))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_panics_for_non_https_address_outside_dev() {
        let addr = Url::parse("http://localhost:8200").unwrap();
        let result = std::panic::catch_unwind(|| {
            assert_tls_outside_dev(&addr, Profile::Production)
        });
        assert!(
            result.is_err(),
            "a non-https VAULT_ADDR outside profile=dev must panic"
        );
    }

    #[test]
    fn guard_allows_non_https_address_in_dev() {
        let addr = Url::parse("http://localhost:8200").unwrap();
        assert_tls_outside_dev(&addr, Profile::Dev); // must not panic
    }

    #[test]
    fn guard_allows_https_address_outside_dev() {
        let addr = Url::parse("https://vault.example.com:8200").unwrap();
        assert_tls_outside_dev(&addr, Profile::Production); // must not panic
    }
}

//! The per-tenant HMAC pepper's lifecycle (DESIGN.md §7.6): minted once via
//! the tenant's own Transit key, the wrapped ciphertext persisted on
//! `tenant.vault_pepper_wrapped`, unwrapped plaintext handed to the caller
//! to cache (`key_cache::KeyCache`, keyed by `tenant_id`) — never persisted
//! or logged in plaintext.

use sqlx::PgPool;
use zeroize::Zeroizing;

use crate::keystore::{KeyStore, KeyStoreError};
use crate::tenant::model::Tenant;
use crate::tenant::repo as tenant_repo;

#[derive(Debug)]
pub enum TenantPepperError {
    Database(sqlx::Error),
    Vault(KeyStoreError),
}

impl std::fmt::Display for TenantPepperError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(err) => {
                write!(f, "tenant pepper operation failed (database): {err}")
            }
            Self::Vault(err) => {
                write!(f, "tenant pepper operation failed (vault): {err}")
            }
        }
    }
}

impl std::error::Error for TenantPepperError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(err) => Some(err),
            Self::Vault(err) => Some(err),
        }
    }
}

impl From<sqlx::Error> for TenantPepperError {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err)
    }
}

impl From<KeyStoreError> for TenantPepperError {
    fn from(err: KeyStoreError) -> Self {
        Self::Vault(err)
    }
}

/// Returns `tenant`'s HMAC pepper, minting and persisting a wrapped one on
/// first call and unwrapping the persisted one on every call after —
/// mirrors `tenant_config`'s "no auto-seeding, lazy on first use" precedent
/// (T-007 decision 4), applied to a Vault-backed secret instead of a config
/// row. Uses `tenant.vault_mount` — the exact same Transit key DEKs use, no
/// second key and no ACL change.
pub async fn ensure_tenant_pepper(
    control_pool: &PgPool,
    keystore: &dyn KeyStore,
    tenant: &Tenant,
) -> Result<Zeroizing<Vec<u8>>, TenantPepperError> {
    match &tenant.vault_pepper_wrapped {
        Some(wrapped) => Ok(keystore.unwrap_dek(&tenant.vault_mount, wrapped).await?),
        None => {
            let secret = keystore.create_dek(&tenant.vault_mount).await?;
            tenant_repo::record_vault_pepper_wrapped(
                control_pool,
                tenant.id,
                &secret.wrapped,
            )
            .await?;
            Ok(secret.plaintext)
        }
    }
}

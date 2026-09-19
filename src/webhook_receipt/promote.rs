//! Orchestrates webhook-receipt promotion (T-047, DESIGN.md §10): for each
//! pending `webhook_receipt_staging` row, matches it against
//! `comms_event.provider_ref` (reusing `orphan_reconcile::repo::find_match`)
//! and either encrypts it into `comms_event` under the matched customer's
//! DEK, or -- no match yet -- hands it to `orphan_event` for T-030's
//! existing reconciler to pick up on its own schedule. One-shot
//! `messgr-control` subcommand invoked by cron (decision 5), mirroring
//! `orphan_reconcile::reconcile::run_for_tenant`'s resolve/connect/close
//! shape.

use sqlx::PgPool;

use crate::customer_dek::lifecycle::{CustomerDekError, get_or_create_dek};
use crate::encryption::{self, EncryptionError};
use crate::key_cache::KeyCache;
use crate::keystore::{KeyStore, KeyStoreError};
use crate::orphan_reconcile::reconcile::{is_recognized_event_type, should_advance};
use crate::orphan_reconcile::repo::{self as orphan_repo, Match};
use crate::tenant::pool::connect_tenant_pool;
use crate::tenant::repo as tenant_repo;

use super::repo::{self, PendingStagingReceipt};

#[derive(Debug)]
pub enum WebhookPromoteError {
    Database(sqlx::Error),
    Vault(KeyStoreError),
    Encryption(EncryptionError),
    UnknownTenant(String),
}

impl std::fmt::Display for WebhookPromoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(err) => {
                write!(f, "webhook promote failed (database): {err}")
            }
            Self::Vault(err) => write!(f, "webhook promote failed (vault): {err}"),
            Self::Encryption(err) => {
                write!(f, "webhook promote failed (encryption): {err}")
            }
            Self::UnknownTenant(slug) => {
                write!(f, "no tenant registered with slug {slug:?}")
            }
        }
    }
}

impl std::error::Error for WebhookPromoteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(err) => Some(err),
            Self::Vault(err) => Some(err),
            Self::Encryption(err) => Some(err),
            Self::UnknownTenant(_) => None,
        }
    }
}

impl From<sqlx::Error> for WebhookPromoteError {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err)
    }
}

impl From<CustomerDekError> for WebhookPromoteError {
    fn from(err: CustomerDekError) -> Self {
        match err {
            CustomerDekError::Database(err) => Self::Database(err),
            CustomerDekError::Vault(err) => Self::Vault(err),
        }
    }
}

impl From<EncryptionError> for WebhookPromoteError {
    fn from(err: EncryptionError) -> Self {
        Self::Encryption(err)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PromoteReport {
    pub promoted: u64,
    pub orphaned: u64,
}

/// Resolves `tenant_slug`, opens its pool, and promotes every pending
/// `webhook_receipt_staging` row.
pub async fn run_for_tenant(
    control_pool: &PgPool,
    control_database_url: &str,
    tenant_slug: &str,
    keystore: &dyn KeyStore,
    database_max_connections: u32,
) -> Result<PromoteReport, WebhookPromoteError> {
    let tenant = tenant_repo::find_by_slug(control_pool, tenant_slug)
        .await?
        .ok_or_else(|| WebhookPromoteError::UnknownTenant(tenant_slug.to_string()))?;

    let tenant_pool = connect_tenant_pool(
        control_pool,
        control_database_url,
        tenant.id,
        &tenant.database_name,
        database_max_connections,
    )
    .await?;

    // A single one-shot run, not a long-lived process -- same sizing
    // rationale as `orphan_reconcile::reconcile::run_for_tenant`'s own
    // cache.
    let cache = KeyCache::new(
        std::num::NonZeroUsize::new(1000).expect("1000 is nonzero"),
        std::time::Duration::from_secs(60),
    );

    let result = run(&tenant_pool.pool, keystore, &cache, &tenant.vault_mount).await;

    tenant_pool.pool.close().await;
    result
}

async fn run(
    tenant_pool: &PgPool,
    keystore: &dyn KeyStore,
    cache: &KeyCache,
    vault_mount: &str,
) -> Result<PromoteReport, WebhookPromoteError> {
    let mut report = PromoteReport::default();

    for receipt in repo::list_pending(tenant_pool).await? {
        // Same regression-relevant safety net as orphan_reconcile: an
        // event_type outside the documented set must never be promoted --
        // route it to orphan_event, where it gets the identical treatment
        // (and, eventually, ages out) a row that missed on its first
        // reconcile attempt would.
        let m = if is_recognized_event_type(&receipt.event_type) {
            orphan_repo::find_match(tenant_pool, &receipt.provider_ref).await?
        } else {
            None
        };

        match m {
            Some(m) => {
                promote_match(tenant_pool, keystore, cache, vault_mount, &receipt, m)
                    .await?;
                report.promoted += 1;
            }
            None => {
                repo::to_orphan(tenant_pool, &receipt).await?;
                report.orphaned += 1;
            }
        }
    }

    Ok(report)
}

async fn promote_match(
    tenant_pool: &PgPool,
    keystore: &dyn KeyStore,
    cache: &KeyCache,
    vault_mount: &str,
    receipt: &PendingStagingReceipt,
    m: Match,
) -> Result<(), WebhookPromoteError> {
    let dek =
        get_or_create_dek(tenant_pool, keystore, cache, vault_mount, m.customer_id)
            .await?;
    let plaintext = serde_json::to_vec(&receipt.provider_payload_raw)
        .expect("jsonb always serializes");
    let ciphertext =
        encryption::encrypt(&dek, m.comms_request_id.as_bytes(), &plaintext)?;

    let advance =
        should_advance(m.current_final_status.as_deref(), &receipt.event_type);
    repo::promote(tenant_pool, receipt, &m, Some(ciphertext), advance).await?;
    Ok(())
}

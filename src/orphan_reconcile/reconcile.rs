//! Orchestrates orphan-event reconciliation (DESIGN.md §4.4/§10, T-022
//! decision 5, T-030): periodically re-matches pending `orphan_event` rows
//! against `comms_event.provider_ref`, promoting a match into a real
//! `comms_event` row -- encrypted under the matched customer's DEK, since
//! this is the payload's first write under a customer scope (AGENTS.md hard
//! invariant 7) -- and ageing out rows past `RECONCILE_ATTEMPTS_CAP`.

use sqlx::PgPool;

use crate::customer_dek::lifecycle::{CustomerDekError, get_or_create_dek};
use crate::encryption::{self, EncryptionError};
use crate::key_cache::KeyCache;
use crate::keystore::{KeyStore, KeyStoreError};
use crate::tenant::pool::connect_tenant_pool;
use crate::tenant::repo as tenant_repo;

use super::repo::{self, Match, PendingOrphan};

/// A one-shot `messgr-control` subcommand invoked by cron (mirrors T-029's
/// shape), not a continuous poll loop -- 5 attempts means 5 separate cron
/// invocations before an unmatched receipt is given up on. Tune later
/// against the real cron interval once `messgr-webhook` gives real traffic
/// to observe.
pub const RECONCILE_ATTEMPTS_CAP: i16 = 5;

const STATUS_ORDER: &[&str] = &["queued", "sent", "delivered", "read"];
const ABSORBING_STATUSES: &[&str] = &[
    "failed",
    "bounced",
    "complaint",
    "expired",
    "cancelled",
    "suppressed_consent",
    "suppressed_list",
    "unverified_address",
];

/// T-034 F4: the only set of values that may reach `comms_request.final_status`
/// via promotion -- `orphan.event_type` can originate from third-party
/// (`messgr-webhook`) input, unlike `dispatcher::repo::write_terminal`'s
/// dispatcher-internal argument of the same name.
fn is_recognized_event_type(event_type: &str) -> bool {
    STATUS_ORDER.contains(&event_type) || ABSORBING_STATUSES.contains(&event_type)
}

/// Decision 3: `final_status` only ever advances. No current status always
/// advances; an absorbing status always wins regardless of the current one
/// (compliance-relevant, must never be silently dropped by an earlier, more
/// benign status); otherwise only a strictly later point in `STATUS_ORDER`
/// advances -- guarding against a late, out-of-order receipt regressing an
/// already-more-final status, the whole reason `orphan_event` exists.
fn should_advance(current: Option<&str>, new_event_type: &str) -> bool {
    match current {
        None => true,
        Some(_) if ABSORBING_STATUSES.contains(&new_event_type) => true,
        Some(cur) => matches!(
            (
                STATUS_ORDER.iter().position(|s| *s == cur),
                STATUS_ORDER.iter().position(|s| *s == new_event_type),
            ),
            (Some(c), Some(n)) if n > c
        ),
    }
}

#[derive(Debug)]
pub enum OrphanReconcileError {
    Database(sqlx::Error),
    Vault(KeyStoreError),
    Encryption(EncryptionError),
    UnknownTenant(String),
}

impl std::fmt::Display for OrphanReconcileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(err) => {
                write!(f, "orphan reconcile failed (database): {err}")
            }
            Self::Vault(err) => write!(f, "orphan reconcile failed (vault): {err}"),
            Self::Encryption(err) => {
                write!(f, "orphan reconcile failed (encryption): {err}")
            }
            Self::UnknownTenant(slug) => {
                write!(f, "no tenant registered with slug {slug:?}")
            }
        }
    }
}

impl std::error::Error for OrphanReconcileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(err) => Some(err),
            Self::Vault(err) => Some(err),
            Self::Encryption(err) => Some(err),
            Self::UnknownTenant(_) => None,
        }
    }
}

impl From<sqlx::Error> for OrphanReconcileError {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err)
    }
}

impl From<CustomerDekError> for OrphanReconcileError {
    fn from(err: CustomerDekError) -> Self {
        match err {
            CustomerDekError::Database(err) => Self::Database(err),
            CustomerDekError::Vault(err) => Self::Vault(err),
        }
    }
}

impl From<EncryptionError> for OrphanReconcileError {
    fn from(err: EncryptionError) -> Self {
        Self::Encryption(err)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ReconcileReport {
    pub reconciled: u64,
    pub aged_out: u64,
    pub still_pending: u64,
}

/// Resolves `tenant_slug`, opens its pool, and reconciles every pending
/// `orphan_event` row -- mirrors `idempotency_sweep::run_for_tenant`'s
/// resolve/connect/close shape.
pub async fn run_for_tenant(
    control_pool: &PgPool,
    control_database_url: &str,
    tenant_slug: &str,
    keystore: &dyn KeyStore,
    database_max_connections: u32,
) -> Result<ReconcileReport, OrphanReconcileError> {
    let tenant = tenant_repo::find_by_slug(control_pool, tenant_slug)
        .await?
        .ok_or_else(|| OrphanReconcileError::UnknownTenant(tenant_slug.to_string()))?;

    let tenant_pool = connect_tenant_pool(
        control_pool,
        control_database_url,
        tenant.id,
        &tenant.database_name,
        database_max_connections,
    )
    .await?;

    // A single one-shot run, not a long-lived process -- small fixed
    // capacity and a short TTL, matching the reasoning `customer_dek`'s own
    // `KeyCache` sizing note gives for a caller with no load history yet.
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
) -> Result<ReconcileReport, OrphanReconcileError> {
    let mut report = ReconcileReport::default();

    for orphan in repo::list_pending(tenant_pool).await? {
        let m = if is_recognized_event_type(&orphan.event_type) {
            repo::find_match(tenant_pool, &orphan.provider_ref).await?
        } else {
            None
        };
        match m {
            Some(m) => {
                promote_match(tenant_pool, keystore, cache, vault_mount, &orphan, m)
                    .await?;
                report.reconciled += 1;
            }
            None if orphan.reconcile_attempts + 1 >= RECONCILE_ATTEMPTS_CAP => {
                repo::record_miss(tenant_pool, orphan.id, RECONCILE_ATTEMPTS_CAP)
                    .await?;
                report.aged_out += 1;
            }
            None => {
                repo::record_miss(tenant_pool, orphan.id, RECONCILE_ATTEMPTS_CAP)
                    .await?;
                report.still_pending += 1;
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
    orphan: &PendingOrphan,
    m: Match,
) -> Result<(), OrphanReconcileError> {
    let ciphertext = match &orphan.provider_payload_raw {
        Some(raw) => {
            let dek = get_or_create_dek(
                tenant_pool,
                keystore,
                cache,
                vault_mount,
                m.customer_id,
            )
            .await?;
            let plaintext = serde_json::to_vec(raw).expect("jsonb always serializes");
            let blob =
                encryption::encrypt(&dek, m.comms_request_id.as_bytes(), &plaintext)?;
            Some(blob)
        }
        None => None,
    };

    let advance = should_advance(m.current_final_status.as_deref(), &orphan.event_type);
    repo::promote(tenant_pool, orphan, &m, ciphertext, advance).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_status_order_value_is_recognized() {
        for status in STATUS_ORDER {
            assert!(is_recognized_event_type(status));
        }
    }

    #[test]
    fn every_absorbing_status_is_recognized() {
        for status in ABSORBING_STATUSES {
            assert!(is_recognized_event_type(status));
        }
    }

    #[test]
    fn an_unrecognized_event_type_is_rejected() {
        assert!(!is_recognized_event_type("made_up_status"));
    }

    #[test]
    fn an_empty_event_type_is_rejected() {
        assert!(!is_recognized_event_type(""));
    }
}

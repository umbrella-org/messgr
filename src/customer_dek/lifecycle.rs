use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::key_cache::KeyCache;
use crate::keystore::{KeyStore, KeyStoreError};

use super::repo;

#[derive(Debug)]
pub enum CustomerDekError {
    Database(sqlx::Error),
    Vault(KeyStoreError),
}

impl std::fmt::Display for CustomerDekError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(err) => {
                write!(f, "customer_dek operation failed (database): {err}")
            }
            Self::Vault(err) => {
                write!(f, "customer_dek operation failed (vault): {err}")
            }
        }
    }
}

impl std::error::Error for CustomerDekError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(err) => Some(err),
            Self::Vault(err) => Some(err),
        }
    }
}

impl From<sqlx::Error> for CustomerDekError {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err)
    }
}

impl From<KeyStoreError> for CustomerDekError {
    fn from(err: KeyStoreError) -> Self {
        Self::Vault(err)
    }
}

/// Returns `customer_id`'s plaintext DEK: a cache hit if warm, otherwise an
/// unwrap of the persisted row if one exists, otherwise a fresh Transit
/// datakey — created and persisted before this call returns, so a second
/// call (even from a different process) never mints a second DEK for the
/// same customer (the race is resolved by `repo::insert_if_absent`, not by
/// this function's own control flow).
pub async fn get_or_create_dek(
    pool: &PgPool,
    keystore: &dyn KeyStore,
    cache: &KeyCache,
    mount: &str,
    customer_id: Uuid,
) -> Result<Zeroizing<Vec<u8>>, CustomerDekError> {
    if let Some(plaintext) = cache.get(customer_id) {
        return Ok(plaintext);
    }

    let plaintext = match repo::find(pool, customer_id).await? {
        Some(row) => keystore.unwrap_dek(mount, &row.wrapped_dek).await?,
        None => {
            let dek = keystore.create_dek(mount).await?;
            if repo::insert_if_absent(pool, customer_id, &dek.wrapped, Utc::now())
                .await?
            {
                dek.plaintext
            } else {
                // Lost the race to a concurrent creator — use the winner's
                // row, not the key we just generated and discarded.
                let row = repo::find(pool, customer_id).await?.expect(
                    "a row must exist immediately after losing insert_if_absent's race",
                );
                keystore.unwrap_dek(mount, &row.wrapped_dek).await?
            }
        }
    };

    cache.put(customer_id, plaintext.clone());
    Ok(plaintext)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreProvisionOutcome {
    pub created: usize,
    pub already_existed: usize,
}

/// Ensures every id in `customer_ids` has a `customer_dek` row, creating one
/// for whichever don't. Caller-supplied ids only — this function never
/// queries a customer table (the customer projection doesn't exist yet, see
/// T-008's Description). Idempotent: re-running with the same ids reports
/// everything as `already_existed` and mints nothing new.
pub async fn pre_provision_deks(
    pool: &PgPool,
    keystore: &dyn KeyStore,
    mount: &str,
    customer_ids: &[Uuid],
) -> Result<PreProvisionOutcome, CustomerDekError> {
    let mut outcome = PreProvisionOutcome {
        created: 0,
        already_existed: 0,
    };

    for &customer_id in customer_ids {
        if repo::find(pool, customer_id).await?.is_some() {
            outcome.already_existed += 1;
            continue;
        }

        let dek = keystore.create_dek(mount).await?;
        if repo::insert_if_absent(pool, customer_id, &dek.wrapped, Utc::now()).await? {
            outcome.created += 1;
        } else {
            outcome.already_existed += 1;
        }
    }

    Ok(outcome)
}

/// CLI-facing wrapper: resolves `tenant_slug`, opens its pool, runs
/// `pre_provision_deks`, and writes one `customer_dek.pre_provision`
/// `platform_audit` row — mirrors `tenant_config::configure::set_tenant_config`'s
/// shape.
pub async fn pre_provision_for_tenant(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    keystore: &dyn KeyStore,
    customer_ids: &[Uuid],
    actor: &str,
) -> Result<PreProvisionOutcome, CustomerDekError> {
    let tenant = crate::tenant::repo::find_by_slug(control_pool, tenant_slug)
        .await?
        .ok_or_else(|| {
            CustomerDekError::Database(sqlx::Error::Configuration(
                format!("no tenant registered with slug {tenant_slug:?}").into(),
            ))
        })?;

    let tenant_pool = crate::tenant::pool::connect_tenant_pool(
        control_pool,
        base_db_url,
        tenant.id,
        &tenant.database_name,
        5,
    )
    .await?;

    let result = pre_provision_deks(
        &tenant_pool.pool,
        keystore,
        &tenant.vault_mount,
        customer_ids,
    )
    .await;
    tenant_pool.pool.close().await;
    let outcome = result?;

    crate::platform_audit::record(
        control_pool,
        actor,
        "customer_dek.pre_provision",
        Some(tenant.id),
        serde_json::json!({
            "requested": customer_ids.len(),
            "created": outcome.created,
            "already_existed": outcome.already_existed,
        }),
    )
    .await?;

    Ok(outcome)
}

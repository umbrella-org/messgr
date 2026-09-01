//! Customer + address resolution at ingest (DESIGN.md §4.6, §4.7, T-015).
//! Never rejects a send for lack of a match — a provisional shell is minted
//! instead (§4.7: "never reject a send because resolution failed").

use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

use crate::customer_dek::lifecycle::{CustomerDekError, get_or_create_dek};
use crate::destination_hmac;
use crate::encryption::{self, EncryptionError};
use crate::key_cache::KeyCache;
use crate::keystore::{KeyStore, KeyStoreError};

use super::model::{self, Customer};
use super::repo;

pub enum ResolutionInput {
    Explicit(Uuid),
    External { system: String, external_id: String },
    AddressOnly,
}

#[derive(Debug)]
pub struct Resolved {
    pub customer_id: Uuid,
    pub address_id: Uuid,
    /// `Some` only for a non-provisional customer — a provisional row's
    /// `locale` is just the tenant default it was minted with, so falling
    /// through to the tenant default is equivalent and clearer about why.
    pub locale: Option<String>,
}

#[derive(Debug)]
pub enum ResolveError {
    Database(sqlx::Error),
    Vault(KeyStoreError),
    Encryption(EncryptionError),
    /// The destination's `value_hmac` is already active under a different
    /// customer — a genuine identity conflict, not absence (decision 9).
    AddressConflict {
        existing_customer_id: Uuid,
    },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(err) => {
                write!(f, "customer resolution failed (database): {err}")
            }
            Self::Vault(err) => write!(f, "customer resolution failed (vault): {err}"),
            Self::Encryption(err) => {
                write!(f, "customer resolution failed (encryption): {err}")
            }
            Self::AddressConflict {
                existing_customer_id,
            } => write!(
                f,
                "destination is already active under a different customer ({existing_customer_id})"
            ),
        }
    }
}

impl std::error::Error for ResolveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(err) => Some(err),
            Self::Vault(err) => Some(err),
            Self::Encryption(err) => Some(err),
            Self::AddressConflict { .. } => None,
        }
    }
}

impl From<sqlx::Error> for ResolveError {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err)
    }
}

impl From<KeyStoreError> for ResolveError {
    fn from(err: KeyStoreError) -> Self {
        Self::Vault(err)
    }
}

impl From<EncryptionError> for ResolveError {
    fn from(err: EncryptionError) -> Self {
        Self::Encryption(err)
    }
}

impl From<CustomerDekError> for ResolveError {
    fn from(err: CustomerDekError) -> Self {
        match err {
            CustomerDekError::Database(err) => Self::Database(err),
            CustomerDekError::Vault(err) => Self::Vault(err),
        }
    }
}

fn non_provisional_locale(customer: &Customer) -> Option<String> {
    if customer.provisional {
        None
    } else {
        Some(customer.locale.clone())
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn resolve(
    pool: &PgPool,
    keystore: &dyn KeyStore,
    dek_cache: &KeyCache,
    vault_mount: &str,
    pepper: &[u8],
    input: ResolutionInput,
    destination: &str,
    channel: &str,
    tenant_default_locale: &str,
    tenant_default_timezone: &str,
) -> Result<Resolved, ResolveError> {
    let kind = model::kind_for_channel(channel);
    let value_hmac = destination_hmac::compute(pepper, destination);

    let (customer_id, locale) = match input {
        ResolutionInput::Explicit(id) => {
            let id = repo::expand_alias(pool, id).await?;
            match repo::find_by_id(pool, id).await? {
                Some(row) => (id, non_provisional_locale(&row)),
                None => {
                    mint_provisional_customer(
                        pool,
                        id,
                        tenant_default_locale,
                        tenant_default_timezone,
                    )
                    .await?;
                    (id, None)
                }
            }
        }
        ResolutionInput::External {
            system,
            external_id,
        } => match repo::find_customer_by_external_id(pool, &system, &external_id)
            .await?
        {
            Some(id) => {
                let row = repo::find_by_id(pool, id).await?.expect(
                    "a customer row must exist for an id found via customer_external_id",
                );
                (id, non_provisional_locale(&row))
            }
            None => {
                let id = mint_provisional_customer_with_external_id(
                    pool,
                    &system,
                    &external_id,
                    tenant_default_locale,
                    tenant_default_timezone,
                )
                .await?;
                (id, None)
            }
        },
        ResolutionInput::AddressOnly => {
            if let Some(address) =
                repo::find_active_address_by_hmac(pool, kind, &value_hmac).await?
            {
                return Ok(Resolved {
                    customer_id: address.customer_id,
                    address_id: address.id,
                    locale: match repo::find_by_id(pool, address.customer_id).await? {
                        Some(row) => non_provisional_locale(&row),
                        None => None,
                    },
                });
            }

            let (customer_id, address_id) = mint_provisional_customer_and_address(
                pool,
                keystore,
                dek_cache,
                vault_mount,
                kind,
                &value_hmac,
                destination,
                tenant_default_locale,
                tenant_default_timezone,
            )
            .await?;
            return Ok(Resolved {
                customer_id,
                address_id,
                locale: None,
            });
        }
    };

    // Explicit/External paths only, reached when the branch above didn't
    // already return: find or create the address for this customer.
    if let Some(address) =
        repo::find_active_address_for_customer(pool, customer_id, kind, &value_hmac)
            .await?
    {
        return Ok(Resolved {
            customer_id,
            address_id: address.id,
            locale,
        });
    }

    let dek =
        get_or_create_dek(pool, keystore, dek_cache, vault_mount, customer_id).await?;
    let address_id = Uuid::new_v4();
    let ciphertext =
        encryption::encrypt(&dek, address_id.as_bytes(), destination.as_bytes())?;
    let now = Utc::now();

    let mut tx = pool.begin().await?;
    let rank = repo::next_rank_for_update(&mut tx, customer_id, kind).await?;
    let inserted = repo::insert_address(
        &mut tx,
        address_id,
        customer_id,
        kind,
        &ciphertext,
        &value_hmac,
        rank,
        now,
        now,
    )
    .await?;

    if inserted {
        tx.commit().await?;
        return Ok(Resolved {
            customer_id,
            address_id,
            locale,
        });
    }

    // Decision 9: the (kind, value_hmac) index rejected the insert. Whether
    // this is a lost race against ourselves (same customer_id) or a genuine
    // conflict with a different customer is decided by the re-fetch below.
    tx.rollback().await?;
    let winner = repo::find_active_address_by_hmac(pool, kind, &value_hmac)
        .await?
        .expect("a row must exist immediately after losing insert_address's race");
    if winner.customer_id != customer_id {
        return Err(ResolveError::AddressConflict {
            existing_customer_id: winner.customer_id,
        });
    }
    Ok(Resolved {
        customer_id,
        address_id: winner.id,
        locale,
    })
}

/// Two concurrent `Explicit(id)` resolutions for the same never-before-seen
/// id can both reach this point (review finding F1): `insert_customer`'s
/// `ON CONFLICT (id) DO NOTHING` makes the loser's insert a no-op instead of
/// a raw unique-violation propagating as a `500`. Either way, by the time
/// this returns, a customer row exists under `id` — which is all the caller
/// needs, since it already holds `id` and both racers would have minted the
/// same provisional shape (decision 6).
async fn mint_provisional_customer(
    pool: &PgPool,
    id: Uuid,
    tenant_default_locale: &str,
    tenant_default_timezone: &str,
) -> Result<(), ResolveError> {
    let mut tx = pool.begin().await?;
    repo::insert_customer(
        &mut tx,
        id,
        tenant_default_locale,
        tenant_default_timezone,
        true,
        None,
        Utc::now(),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn mint_provisional_customer_with_external_id(
    pool: &PgPool,
    system: &str,
    external_id: &str,
    tenant_default_locale: &str,
    tenant_default_timezone: &str,
) -> Result<Uuid, ResolveError> {
    let id = Uuid::new_v4();
    let now = Utc::now();

    let mut tx = pool.begin().await?;
    repo::insert_customer(
        &mut tx,
        id,
        tenant_default_locale,
        tenant_default_timezone,
        true,
        None,
        now,
    )
    .await?;
    let inserted = repo::insert_external_id(&mut tx, system, external_id, id).await?;

    if inserted {
        tx.commit().await?;
        return Ok(id);
    }

    tx.rollback().await?;
    let winner = repo::find_customer_by_external_id(pool, system, external_id)
        .await?
        .expect("a row must exist immediately after losing insert_external_id's race");
    Ok(winner)
}

#[allow(clippy::too_many_arguments)]
async fn mint_provisional_customer_and_address(
    pool: &PgPool,
    keystore: &dyn KeyStore,
    dek_cache: &KeyCache,
    vault_mount: &str,
    kind: &str,
    value_hmac: &[u8],
    destination: &str,
    tenant_default_locale: &str,
    tenant_default_timezone: &str,
) -> Result<(Uuid, Uuid), ResolveError> {
    let customer_id = Uuid::new_v4();
    let address_id = Uuid::new_v4();
    let now = Utc::now();

    let dek =
        get_or_create_dek(pool, keystore, dek_cache, vault_mount, customer_id).await?;
    let ciphertext =
        encryption::encrypt(&dek, address_id.as_bytes(), destination.as_bytes())?;

    let mut tx = pool.begin().await?;
    repo::insert_customer(
        &mut tx,
        customer_id,
        tenant_default_locale,
        tenant_default_timezone,
        true,
        None,
        now,
    )
    .await?;
    let inserted = repo::insert_address(
        &mut tx,
        address_id,
        customer_id,
        kind,
        &ciphertext,
        value_hmac,
        1,
        now,
        now,
    )
    .await?;

    if inserted {
        tx.commit().await?;
        return Ok((customer_id, address_id));
    }

    tx.rollback().await?;
    let winner = repo::find_active_address_by_hmac(pool, kind, value_hmac)
        .await?
        .expect("a row must exist immediately after losing insert_address's race");
    Ok((winner.customer_id, winner.id))
}

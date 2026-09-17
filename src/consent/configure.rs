use chrono::Utc;
use sqlx::PgPool;

use crate::customer::model::kind_for_channel;
use crate::customer::repo as customer_repo;
use crate::destination_hmac;
use crate::keystore::{KeyStore, KeyStoreError};
use crate::tenant::pool::connect_tenant_pool;
use crate::tenant::repo as tenant_repo;
use crate::tenant_pepper::{TenantPepperError, ensure_tenant_pepper};

use super::model::ConsentInput;
use super::repo;

/// A `set` run can fail on the control-database side, the tenant-database
/// side, the Vault side (unwrapping the tenant pepper), or a domain-level
/// rejection (an unknown `tenant_slug`, or a destination with no active
/// address on file) -- the last of these is carried as
/// `sqlx::Error::Configuration`, matching `suppression::configure::ConfigureError`'s
/// shape.
#[derive(Debug)]
pub enum ConfigureError {
    Database(sqlx::Error),
    Vault(KeyStoreError),
}

impl std::fmt::Display for ConfigureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(err) => write!(f, "consent operation failed: {err}"),
            Self::Vault(err) => write!(f, "consent operation failed (vault): {err}"),
        }
    }
}

impl std::error::Error for ConfigureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(err) => Some(err),
            Self::Vault(err) => Some(err),
        }
    }
}

impl From<sqlx::Error> for ConfigureError {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err)
    }
}

impl From<KeyStoreError> for ConfigureError {
    fn from(err: KeyStoreError) -> Self {
        Self::Vault(err)
    }
}

impl From<TenantPepperError> for ConfigureError {
    fn from(err: TenantPepperError) -> Self {
        match err {
            TenantPepperError::Database(err) => Self::Database(err),
            TenantPepperError::Vault(err) => Self::Vault(err),
        }
    }
}

fn rejected(message: String) -> ConfigureError {
    sqlx::Error::Configuration(message.into()).into()
}

#[derive(Debug)]
pub struct ConfigureOutcome {
    /// "created" | "updated" | "idempotent" -- never "rejected", which is
    /// returned as an `Err` instead.
    pub outcome: &'static str,
}

/// Resolves `tenant_slug`, opens its pool, derives the destination's active
/// `customer_address` via the tenant pepper (T-037 decision 1 -- never
/// mints one: rejected if none exists), and upserts a `consent` row keyed on
/// that address's id + `class`, auditing exactly one `consent.set`
/// `platform_audit` row with outcome `created`/`updated`/`idempotent`/
/// `rejected`.
///
/// Closes the tenant pool on every exit path (T-037 decision 7).
#[allow(clippy::too_many_arguments)]
pub async fn set_consent(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    keystore: &dyn KeyStore,
    destination: &str,
    channel: &str,
    class: &str,
    opted_in: bool,
    source: &str,
    actor: &str,
) -> Result<ConfigureOutcome, ConfigureError> {
    let tenant = match tenant_repo::find_by_slug(control_pool, tenant_slug).await? {
        Some(tenant) => tenant,
        None => {
            audit(
                control_pool,
                actor,
                None,
                None,
                class,
                opted_in,
                source,
                "rejected",
            )
            .await?;
            return Err(rejected(format!(
                "no tenant registered with slug {tenant_slug:?}"
            )));
        }
    };

    let tenant_pool = connect_tenant_pool(
        control_pool,
        base_db_url,
        tenant.id,
        &tenant.database_name,
        5,
    )
    .await?;

    let pepper = match ensure_tenant_pepper(control_pool, keystore, &tenant).await {
        Ok(pepper) => pepper,
        Err(err) => {
            tenant_pool.pool.close().await;
            return Err(err.into());
        }
    };
    let value_hmac = destination_hmac::compute(&pepper, destination);
    let kind = kind_for_channel(channel);

    let address = match customer_repo::find_active_address_by_hmac(
        &tenant_pool.pool,
        kind,
        &value_hmac,
    )
    .await
    {
        Ok(address) => address,
        Err(err) => {
            tenant_pool.pool.close().await;
            return Err(err.into());
        }
    };
    let Some(address) = address else {
        tenant_pool.pool.close().await;
        audit(
            control_pool,
            actor,
            Some(tenant.id),
            None,
            class,
            opted_in,
            source,
            "rejected",
        )
        .await?;
        return Err(rejected(
            "no active address on file for this destination -- consent can only be recorded \
             against an address that has already resolved (e.g. via a prior message); it is \
             never minted by this command"
                .to_string(),
        ));
    };

    let input = ConsentInput {
        address_id: address.id,
        class: class.to_string(),
        opted_in,
        source: source.to_string(),
    };
    let result =
        set_consent_inner(control_pool, &tenant_pool.pool, tenant.id, actor, input)
            .await;
    tenant_pool.pool.close().await;
    result
}

async fn set_consent_inner(
    control_pool: &PgPool,
    tenant_pool: &PgPool,
    tenant_id: uuid::Uuid,
    actor: &str,
    input: ConsentInput,
) -> Result<ConfigureOutcome, ConfigureError> {
    let existing = repo::load_one(tenant_pool, input.address_id, &input.class).await?;

    let outcome = match &existing {
        None => "created",
        Some(existing) if input.matches(existing) => "idempotent",
        Some(_) => "updated",
    };

    if outcome != "idempotent" {
        repo::upsert(tenant_pool, &input, Utc::now()).await?;
    }

    audit(
        control_pool,
        actor,
        Some(tenant_id),
        Some(input.address_id),
        &input.class,
        input.opted_in,
        &input.source,
        outcome,
    )
    .await?;

    Ok(ConfigureOutcome { outcome })
}

/// `address_id` is safe to audit directly (unlike suppression's raw
/// destination) -- it's an opaque UUID, not PII.
#[allow(clippy::too_many_arguments)]
async fn audit(
    control_pool: &PgPool,
    actor: &str,
    tenant_id: Option<uuid::Uuid>,
    address_id: Option<uuid::Uuid>,
    class: &str,
    opted_in: bool,
    source: &str,
    outcome: &str,
) -> Result<(), sqlx::Error> {
    crate::platform_audit::record(
        control_pool,
        actor,
        "consent.set",
        tenant_id,
        serde_json::json!({
            "address_id": address_id,
            "class": class,
            "opted_in": opted_in,
            "source": source,
            "outcome": outcome,
        }),
    )
    .await
}

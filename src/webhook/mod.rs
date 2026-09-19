pub mod handler;

use std::collections::HashMap;
use std::sync::Arc;

use sqlx::PgPool;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::keystore::VaultKeyStore;
use crate::tenant::pool::connect_tenant_pool;
use crate::webhook_verify::WebhookVerifier;

/// A minimal per-tenant pool cache, deliberately not `tenant::registry::TenantRegistry`
/// (T-047 decision 4): `messgr-webhook` resolves tenants by `webhook_token`,
/// not mTLS producer identity, and has no use for `TenantRegistry`'s DEK
/// cache, tenant pepper, or kill-switch poll loop.
///
/// `ponytail: no idle-TTL eviction yet (unlike TenantRegistry's T-031 sweep)
/// -- add one if an offboarded tenant's pool needs dropping without a
/// process restart.`
#[derive(Default)]
pub struct TenantPoolCache {
    pools: RwLock<HashMap<Uuid, PgPool>>,
}

impl TenantPoolCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the cached pool for `tenant_id`, opening and caching one via
    /// `connect_tenant_pool` on a miss.
    pub async fn get_or_open(
        &self,
        control_pool: &PgPool,
        control_database_url: &str,
        tenant_id: Uuid,
        database_name: &str,
        max_connections: u32,
    ) -> Result<PgPool, sqlx::Error> {
        if let Some(pool) = self.pools.read().await.get(&tenant_id) {
            return Ok(pool.clone());
        }

        let mut pools = self.pools.write().await;
        if let Some(pool) = pools.get(&tenant_id) {
            return Ok(pool.clone());
        }

        let tenant_pool = connect_tenant_pool(
            control_pool,
            control_database_url,
            tenant_id,
            database_name,
            max_connections,
        )
        .await?
        .pool;
        pools.insert(tenant_id, tenant_pool.clone());
        Ok(tenant_pool)
    }
}

/// Shared state for every `messgr-webhook` request handler
/// (`axum::extract::State`).
#[derive(Clone)]
pub struct AppState {
    pub control_pool: PgPool,
    pub control_database_url: String,
    pub vault_keystore: Arc<VaultKeyStore>,
    pub verifier: Arc<dyn WebhookVerifier>,
    pub pool_cache: Arc<TenantPoolCache>,
    pub tenant_pool_max_connections: u32,
}

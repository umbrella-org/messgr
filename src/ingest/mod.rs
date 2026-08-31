pub mod handler;
pub mod identity;
pub mod model;
pub mod repo;

use std::sync::Arc;

use sqlx::PgPool;

use crate::keystore::KeyStore;
use crate::profile::Profile;
use crate::tenant::registry::TenantRegistry;

/// Shared state for every `messgr-ingest` request handler (`axum::extract::State`).
#[derive(Clone)]
pub struct AppState {
    pub control_pool: PgPool,
    pub control_database_url: String,
    pub keystore: Arc<dyn KeyStore>,
    pub registry: Arc<TenantRegistry>,
    pub tenant_pool_max_connections: u32,
    pub profile: Profile,
}

pub mod auth_flag;
pub mod buffer;
pub mod handler;
pub mod identity;
pub mod model;
pub mod provider;
pub mod repo;

use std::path::PathBuf;
use std::sync::Arc;

use sqlx::PgPool;

use crate::keystore::KeyStore;
use crate::profile::Profile;
use crate::tenant::registry::TenantRegistry;

use self::auth_flag::AuthEnabledCache;
use self::provider::ProviderConfigCache;

/// Shared state for `messgr-sms-sender`'s one request handler
/// (`axum::extract::State`). Mirrors `ingest::AppState` (decision 2: mTLS/
/// producer/tenant resolution is identical) plus what the OTP fast path
/// itself needs: the fail-open `auth_enabled` cache (decision 8), the
/// last-known-snapshot `provider_config` cache (`provider::ProviderConfigCache`
/// -- keeps provider selection working through a tenant-DB blip, matching
/// decision 9's "the send still succeeds" for the audit write), the
/// provider base URL (a dev stand-in -- `provider_config` has no `base_url`
/// column, matching `messgr-dispatcher`'s own `DISPATCHER_<CHANNEL>_BASE_URL`
/// precedent), and the local-disk buffer path (decision 9).
#[derive(Clone)]
pub struct AppState {
    pub control_pool: PgPool,
    pub control_database_url: String,
    pub keystore: Arc<dyn KeyStore>,
    pub registry: Arc<TenantRegistry>,
    pub tenant_pool_max_connections: u32,
    pub profile: Profile,
    pub auth_flag: Arc<AuthEnabledCache>,
    pub provider_config_cache: Arc<ProviderConfigCache>,
    pub sms_base_url: String,
    pub buffer_path: PathBuf,
}

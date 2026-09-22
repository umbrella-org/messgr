pub mod auth_flag;
pub mod buffer;
pub mod handler;
pub mod identity;
pub mod model;
pub mod pending;
pub mod provider;
pub mod repo;

use std::path::PathBuf;
use std::sync::Arc;

use sqlx::PgPool;

use crate::keystore::KeyStore;
use crate::tenant::registry::TenantRegistry;

use self::auth_flag::AuthEnabledCache;
use self::provider::OtpProviderCache;

/// Shared state for `messgr-otp`'s one request handler
/// (`axum::extract::State`). Mirrors `sms_sender::AppState` (identical
/// mTLS/producer/tenant resolution, decision 2) with one deliberate swap:
/// `provider_cache` is `OtpProviderCache` (§3.1's at-startup-per-tenant,
/// background-refreshed credential cache), not `sms_sender`'s per-request
/// `ProviderConfigCache`.
#[derive(Clone)]
pub struct AppState {
    pub control_pool: PgPool,
    pub control_database_url: String,
    pub keystore: Arc<dyn KeyStore>,
    pub registry: Arc<TenantRegistry>,
    pub tenant_pool_max_connections: u32,
    pub auth_flag: Arc<AuthEnabledCache>,
    pub provider_cache: Arc<OtpProviderCache>,
    /// Dev stand-in for a real provider, same reasoning as
    /// `sms_sender::AppState::sms_base_url` -- `provider_config` has no
    /// `base_url` column.
    pub otp_base_url: String,
    pub buffer_path: PathBuf,
    pub pending_path: PathBuf,
}

//! At-startup-per-tenant, background-refreshed provider-credential cache
//! (§3.1's correction to `02-otp.md`: `otp-api` must hold its provider
//! credential in memory, refreshed on a background timer, not read Vault on
//! the request path). This is new code, not `sms_sender::provider`'s
//! `ProviderConfigCache` reused -- that cache reads Vault on every send,
//! falling back to the last-known value only on a Vault failure, which is
//! exactly the per-request Vault dependency §3.1 says `otp-api` must not
//! have. Since tenants bring their own provider accounts (decision 18)
//! there is no single credential to prefetch once at boot, so the cache is
//! per-tenant, populated the first time a tenant is seen and refreshed only
//! by its own background timer from then on.
//!
//! The synchronous, in-request provider walk below (decision 7's reasoning,
//! carried over unchanged from `sms_sender::provider::send`): a live
//! customer is waiting, so this tries the next resolved provider
//! immediately on failure rather than the dispatcher's windowed
//! circuit-breaker/half-open-probe behaviour, which is built for queued
//! traffic that can afford to wait.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::keystore::{KeyStore, split_kv_path};
use crate::provider_config::repo as provider_config_repo;
use crate::sender::http::HttpSender;
use crate::sender::{SendOutcome, Sender, SenderError};
use crate::tenant::registry::TenantRegistry;

use super::model::CHANNEL;

/// One `provider_config` row with its Vault credential already resolved.
#[derive(Debug, Clone)]
pub struct ResolvedProviderConfig {
    pub priority: i16,
    pub credential_path: String,
    pub api_key: String,
}

/// Keyed by `tenant_id`; each entry is the resolved provider list for that
/// tenant, populated once (`get_or_fetch`) and refreshed only by
/// `run_refresh_loop`.
pub struct OtpProviderCache {
    entries: RwLock<HashMap<Uuid, Vec<ResolvedProviderConfig>>>,
}

impl OtpProviderCache {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
        }
    }

    /// Returns the cached, already-resolved provider list for `tenant_id`,
    /// resolving it from `pool`/`keystore` exactly once on a cold miss.
    /// Never called from `run_refresh_loop` -- only the first request for a
    /// tenant this process has seen takes this path (mirrors
    /// `TenantRegistry::get_or_open`'s lazy-open shape,
    /// `src/tenant/registry.rs:151`).
    pub async fn get_or_fetch(
        &self,
        pool: &PgPool,
        keystore: &dyn KeyStore,
        tenant_id: Uuid,
    ) -> Vec<ResolvedProviderConfig> {
        if let Some(cached) = self.entries.read().await.get(&tenant_id) {
            return cached.clone();
        }

        // Re-check after acquiring the write lock: two requests can both
        // miss the read-lock check above for the same never-seen tenant.
        let mut entries = self.entries.write().await;
        if let Some(cached) = entries.get(&tenant_id) {
            return cached.clone();
        }

        let resolved = match resolve_all(pool, keystore, tenant_id).await {
            Ok(resolved) => resolved,
            Err(err) => {
                tracing::error!(
                    %err,
                    %tenant_id,
                    "otp: loading provider_config failed on first sight of this tenant, caching an empty provider list"
                );
                Vec::new()
            }
        };
        entries.insert(tenant_id, resolved.clone());
        resolved
    }
}

impl Default for OtpProviderCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Loads `provider_config` rows for `channel = "sms"` and resolves each
/// row's Vault credential. A row whose `credential_path` doesn't parse, or
/// whose Vault read fails, is logged and skipped -- one bad row must not
/// stop the others from resolving.
async fn resolve_all(
    pool: &PgPool,
    keystore: &dyn KeyStore,
    tenant_id: Uuid,
) -> Result<Vec<ResolvedProviderConfig>, sqlx::Error> {
    let configs = provider_config_repo::list(pool, CHANNEL).await?;

    let mut resolved = Vec::with_capacity(configs.len());
    for config in configs {
        let Some((kv_mount, kv_path)) = split_kv_path(&config.credential_path) else {
            tracing::error!(
                credential_path = %config.credential_path,
                priority = config.priority,
                %tenant_id,
                "otp: provider_config.credential_path is not in <mount>/data/<path> form, skipping"
            );
            continue;
        };

        match keystore.read_provider_credential(kv_mount, kv_path).await {
            Ok(api_key) => resolved.push(ResolvedProviderConfig {
                priority: config.priority,
                credential_path: config.credential_path,
                api_key,
            }),
            Err(err) => {
                tracing::error!(
                    %err,
                    credential_path = %config.credential_path,
                    priority = config.priority,
                    %tenant_id,
                    "otp: reading provider credential failed, skipping this provider"
                );
            }
        }
    }

    Ok(resolved)
}

/// One refresh pass over every tenant currently in `cache` (not the
/// registry -- only tenants actually seen). A refresh failure logs and
/// keeps the existing cached entry, the same "serve the last known
/// snapshot" shape `sms_sender::provider::ProviderConfigCache` uses, just on
/// a timer instead of per-request. Split out from `run_refresh_loop` so a
/// test can drive one pass directly instead of waiting out a real
/// `poll_interval`.
pub async fn refresh_once(
    cache: &OtpProviderCache,
    control_pool: &PgPool,
    control_database_url: &str,
    keystore: &dyn KeyStore,
    registry: &TenantRegistry,
    tenant_pool_max_connections: u32,
) {
    let tenant_ids: Vec<Uuid> = cache.entries.read().await.keys().copied().collect();
    for tenant_id in tenant_ids {
        let tenant = match registry
            .get_or_open(
                control_pool,
                control_database_url,
                keystore,
                tenant_id,
                tenant_pool_max_connections,
            )
            .await
        {
            Ok(tenant) => tenant,
            Err(err) => {
                tracing::error!(
                    %err,
                    %tenant_id,
                    "otp: refresh could not open the tenant pool, keeping the existing cached credentials"
                );
                continue;
            }
        };

        match resolve_all(&tenant.pool, keystore, tenant_id).await {
            Ok(resolved) => {
                cache.entries.write().await.insert(tenant_id, resolved);
            }
            Err(err) => {
                tracing::error!(
                    %err,
                    %tenant_id,
                    "otp: refresh failed, keeping the existing cached provider credentials"
                );
            }
        }
    }
}

/// Refreshes forever on `poll_interval` -- see `refresh_once`.
#[allow(clippy::too_many_arguments)]
pub async fn run_refresh_loop(
    cache: Arc<OtpProviderCache>,
    control_pool: PgPool,
    control_database_url: String,
    keystore: Arc<dyn KeyStore>,
    registry: Arc<TenantRegistry>,
    tenant_pool_max_connections: u32,
    poll_interval: Duration,
) {
    loop {
        tokio::time::sleep(poll_interval).await;
        refresh_once(
            &cache,
            &control_pool,
            &control_database_url,
            keystore.as_ref(),
            &registry,
            tenant_pool_max_connections,
        )
        .await;
    }
}

#[derive(Debug)]
pub enum ProviderSendError {
    /// Every resolved provider was tried (or none are cached) and none
    /// produced a successful send. Carries the last transport/provider-level
    /// failure encountered, if any provider got far enough to attempt a
    /// send.
    Exhausted(Option<SenderError>),
}

impl ProviderSendError {
    /// Mirrors `dispatcher::worker::provider_status_of` -- `Some(status)`
    /// for a provider rejection, `None` for everything else (a transport
    /// failure or no attempt at all).
    pub fn provider_status(&self) -> Option<String> {
        match self {
            Self::Exhausted(Some(SenderError::Provider { status, .. })) => {
                Some(status.to_string())
            }
            Self::Exhausted(Some(SenderError::Http(_))) | Self::Exhausted(None) => None,
        }
    }
}

impl std::fmt::Display for ProviderSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exhausted(Some(err)) => {
                write!(f, "every configured provider failed, last error: {err}")
            }
            Self::Exhausted(None) => write!(f, "no usable resolved provider for sms"),
        }
    }
}

impl std::error::Error for ProviderSendError {}

/// Tries each already-resolved provider (in the priority order
/// `provider_config_repo::list` returned) with a fresh `HttpSender` until
/// one succeeds or the list is exhausted. Credential acquisition already
/// happened in `OtpProviderCache` -- this never touches Vault.
pub async fn send(
    resolved: &[ResolvedProviderConfig],
    base_url: &str,
    destination: &str,
    body: &str,
) -> Result<SendOutcome, ProviderSendError> {
    let mut last_err = None;
    for config in resolved {
        let sender = HttpSender::new(base_url.to_string(), config.api_key.clone());
        match sender.send(destination, body).await {
            Ok(outcome) => return Ok(outcome),
            Err(err) => {
                tracing::warn!(
                    %err,
                    priority = config.priority,
                    "otp: provider send failed, trying next provider"
                );
                last_err = Some(err);
            }
        }
    }

    Err(ProviderSendError::Exhausted(last_err))
}

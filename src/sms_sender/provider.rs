//! Synchronous, in-request provider walk (decision 7): a live customer is
//! waiting, so this tries the next `provider_config` row immediately on
//! failure rather than the dispatcher's windowed circuit-breaker/
//! half-open-probe behaviour, which is built for queued traffic that can
//! afford to wait.

use std::collections::HashMap;

use sqlx::PgPool;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::keystore::{KeyStore, KeyStoreError, split_kv_path};
use crate::provider_config::model::ProviderConfig;
use crate::provider_config::repo as provider_config_repo;
use crate::sender::http::HttpSender;
use crate::sender::{SendOutcome, Sender, SenderError};

use super::model::CHANNEL;

/// `provider_config` lives in the tenant database, the same pool the audit
/// write uses -- without this, a tenant-DB outage would break provider
/// *selection* too, contradicting decision 9's "the send still succeeds"
/// (02-otp.md §3) and this ticket's own acceptance test, which requires the
/// provider call to be unaffected by a tenant-DB outage. Caches the last
/// successfully loaded row set per tenant and falls back to it on a load
/// failure -- the same "serve the last known snapshot" shape
/// `AuthEnabledCache`/`KillSwitchCache` already use, just keyed per tenant
/// and refreshed opportunistically (on every send) rather than on a poll
/// timer, since there is no natural place to enumerate every tenant this
/// multi-tenant process might ever serve.
pub struct ProviderConfigCache {
    rows: RwLock<HashMap<Uuid, Vec<ProviderConfig>>>,
}

impl ProviderConfigCache {
    pub fn new() -> Self {
        Self {
            rows: RwLock::new(HashMap::new()),
        }
    }

    /// Loads fresh rows for `tenant_id` from `pool`, caching them for next
    /// time. On a database failure, falls back to the last successfully
    /// loaded snapshot for this tenant (an empty list if none has ever
    /// loaded), so a tenant-DB blip degrades provider selection to "stale
    /// but correct as of the last successful read" rather than blocking a
    /// send that needs no database work of its own. `pub`, not private, so
    /// a caller can prime the cache ahead of `send` (e.g. a test proving
    /// the fallback needs a snapshot to already exist).
    pub async fn load(&self, pool: &PgPool, tenant_id: Uuid) -> Vec<ProviderConfig> {
        match provider_config_repo::list(pool, CHANNEL).await {
            Ok(fresh) => {
                self.rows.write().await.insert(tenant_id, fresh.clone());
                fresh
            }
            Err(err) => {
                tracing::error!(
                    %err,
                    %tenant_id,
                    "sms-sender: loading provider_config failed, falling back to the last known snapshot"
                );
                self.rows
                    .read()
                    .await
                    .get(&tenant_id)
                    .cloned()
                    .unwrap_or_default()
            }
        }
    }
}

impl Default for ProviderConfigCache {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub enum ProviderSendError {
    /// Every `provider_config` row for `sms` was tried (or none exist) and
    /// none produced a successful send. Carries the last transport/
    /// provider-level failure encountered, if any row got far enough to
    /// attempt a send.
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
            Self::Exhausted(None) => write!(f, "no usable provider_config row for sms"),
        }
    }
}

impl std::error::Error for ProviderSendError {}

/// Loads `provider_config` rows for `channel = "sms"` (via `cache`, in
/// priority order) and tries each with a fresh `HttpSender` until one
/// succeeds or the list is exhausted. A row whose `credential_path` doesn't
/// parse, or whose Vault read fails, is logged and skipped -- one bad row
/// must not stop the walk from trying the next.
#[allow(clippy::too_many_arguments)]
pub async fn send(
    cache: &ProviderConfigCache,
    pool: &PgPool,
    tenant_id: Uuid,
    keystore: &dyn KeyStore,
    base_url: &str,
    destination: &str,
    body: &str,
) -> Result<SendOutcome, ProviderSendError> {
    let configs = cache.load(pool, tenant_id).await;

    let mut last_err = None;
    for config in &configs {
        let Some((kv_mount, kv_path)) = split_kv_path(&config.credential_path) else {
            tracing::error!(
                credential_path = %config.credential_path,
                priority = config.priority,
                "sms-sender: provider_config.credential_path is not in <mount>/data/<path> form, skipping"
            );
            continue;
        };

        let api_key = match keystore.read_provider_credential(kv_mount, kv_path).await {
            Ok(key) => key,
            Err(err) => {
                log_credential_failure(&err, config.priority);
                continue;
            }
        };

        let sender = HttpSender::new(base_url.to_string(), api_key);
        match sender.send(destination, body).await {
            Ok(outcome) => return Ok(outcome),
            Err(err) => {
                tracing::warn!(
                    %err,
                    priority = config.priority,
                    "sms-sender: provider send failed, trying next provider"
                );
                last_err = Some(err);
            }
        }
    }

    Err(ProviderSendError::Exhausted(last_err))
}

fn log_credential_failure(err: &KeyStoreError, priority: i16) {
    tracing::error!(
        %err,
        priority,
        "sms-sender: reading provider credential failed, trying next provider"
    );
}

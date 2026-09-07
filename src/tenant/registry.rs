//! Lazy, per-process tenant pool + config cache for multi-tenant request-path
//! services (DESIGN.md §2.3: "pools are created lazily... an idle tenant
//! costs no connections"). T-011's `messgr-ingest` is the first caller —
//! unlike a dispatcher (one tenant per process, §2.3), an ingest process
//! serves many tenants and must not reconnect on every request.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use sqlx::PgPool;
use tokio::sync::{Notify, RwLock};
use tokio::task::JoinHandle;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::key_cache::KeyCache;
use crate::keystore::{KeyStore, KeyStoreError};
use crate::kill_switch::cache::{KillSwitchCache, run_refresh_loop};
use crate::tenant::model::Tenant;
use crate::tenant::pool::connect_tenant_pool;
use crate::tenant::repo as tenant_repo;
use crate::tenant_config::model::TenantConfig;
use crate::tenant_config::repo as tenant_config_repo;
use crate::tenant_pepper::{TenantPepperError, ensure_tenant_pepper};

/// `KeyCache`'s own doc comment recommends this exact sizing for "T-011's
/// ingest path", one of the two callers it names.
const DEK_CACHE_CAPACITY: usize = 100_000;
const DEK_CACHE_TTL: Duration = Duration::from_secs(3600);
/// "Every few seconds" (DESIGN.md §5.2) — `messgr-ingest` connects through
/// PgBouncer transaction mode, where `LISTEN` never fires (§2.3), so this
/// poll is the *only* propagation path here, unlike the dispatcher's 30s
/// fallback behind a `LISTEN` fast path.
const KILL_SWITCH_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// No number is given for this anywhere in DESIGN.md — same "pick a
/// reasonable starting point" treatment `DEK_CACHE_TTL` already got (T-031).
pub const TENANT_IDLE_TTL: Duration = Duration::from_secs(3600);
/// A fraction of `TENANT_IDLE_TTL`, so an idle tenant is evicted within at
/// most `TENANT_IDLE_TTL` plus one sweep interval of going idle (T-031).
pub const EVICTION_SWEEP_INTERVAL: Duration = Duration::from_secs(300);

/// Everything a request handler needs for one tenant, opened once and
/// reused for the life of the process, or until evicted by
/// `TenantRegistry::evict_idle` past `TENANT_IDLE_TTL` (T-031) — eviction
/// also cancels the background kill-switch poll loop below, not just the
/// registry's own reference to this context.
pub struct TenantContext {
    pub tenant: Tenant,
    pub pool: PgPool,
    pub config: TenantConfig,
    pub pepper: Zeroizing<Vec<u8>>,
    pub dek_cache: KeyCache,
    /// Refreshed by a background poll spawned the first time this tenant's
    /// context is opened (T-016 decision 6) — never from the dispatcher's
    /// `LISTEN`, which this process cannot use (§2.3).
    pub kill_switches: Arc<KillSwitchCache>,
}

#[derive(Debug)]
pub enum RegistryError {
    /// `resolve_producer` returned a `tenant_id` with no matching `tenant`
    /// row — should be unreachable (the control database is the same one
    /// resolution just queried), but handled rather than unwrapped.
    UnknownTenant(Uuid),
    /// The tenant has never had `tenant_config set` run against it (T-007
    /// decision 4: no auto-seeding).
    NotConfigured(Uuid),
    Database(sqlx::Error),
    Vault(KeyStoreError),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTenant(id) => write!(f, "no tenant row for id {id}"),
            Self::NotConfigured(id) => {
                write!(f, "tenant {id} has no tenant_config row")
            }
            Self::Database(err) => write!(f, "tenant registry database error: {err}"),
            Self::Vault(err) => write!(f, "tenant registry vault error: {err}"),
        }
    }
}

impl std::error::Error for RegistryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(err) => Some(err),
            Self::Vault(err) => Some(err),
            Self::UnknownTenant(_) | Self::NotConfigured(_) => None,
        }
    }
}

impl From<sqlx::Error> for RegistryError {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err)
    }
}

impl From<KeyStoreError> for RegistryError {
    fn from(err: KeyStoreError) -> Self {
        Self::Vault(err)
    }
}

impl From<TenantPepperError> for RegistryError {
    fn from(err: TenantPepperError) -> Self {
        match err {
            TenantPepperError::Database(err) => Self::Database(err),
            TenantPepperError::Vault(err) => Self::Vault(err),
        }
    }
}

/// One tenant's cached context plus the bookkeeping `evict_idle` needs to
/// tear it down: the handle to cancel its poll loop, the task itself (so
/// eviction can wait for it to actually exit), and when it was last touched.
struct RegistryEntry {
    context: Arc<TenantContext>,
    cancel: Arc<Notify>,
    poll_task: JoinHandle<()>,
    last_access: StdMutex<Instant>,
}

pub struct TenantRegistry {
    contexts: RwLock<HashMap<Uuid, RegistryEntry>>,
}

impl TenantRegistry {
    pub fn new() -> Self {
        Self {
            contexts: RwLock::new(HashMap::new()),
        }
    }

    fn touch(entry: &RegistryEntry) {
        *entry
            .last_access
            .lock()
            .expect("TenantRegistry last_access mutex poisoned") = Instant::now();
    }

    /// Returns the cached context for `tenant_id`, opening and caching one
    /// on first use. `control_pool`'s URL doubles as the base for the
    /// tenant pool (`tenant::pool::connect_tenant_pool`) — the same reuse
    /// `pre_provision_for_tenant`/`register_producer` already established,
    /// not a second env var (T-011 decision 8).
    pub async fn get_or_open(
        &self,
        control_pool: &PgPool,
        control_database_url: &str,
        keystore: &dyn KeyStore,
        tenant_id: Uuid,
        max_connections: u32,
    ) -> Result<Arc<TenantContext>, RegistryError> {
        if let Some(entry) = self.contexts.read().await.get(&tenant_id) {
            Self::touch(entry);
            return Ok(entry.context.clone());
        }

        // Re-check after acquiring the write lock: two requests can both
        // miss the read-lock check above for the same never-seen tenant.
        let mut contexts = self.contexts.write().await;
        if let Some(entry) = contexts.get(&tenant_id) {
            Self::touch(entry);
            return Ok(entry.context.clone());
        }

        let tenant = tenant_repo::find_by_id(control_pool, tenant_id)
            .await?
            .ok_or(RegistryError::UnknownTenant(tenant_id))?;

        let tenant_pool = connect_tenant_pool(
            control_pool,
            control_database_url,
            tenant_id,
            &tenant.database_name,
            max_connections,
        )
        .await?;

        // `tenant_config` is a tenant-database table (§4.10 — one row per
        // tenant, no `tenant_id` column, the database itself is the
        // tenant), so it's loaded from the pool just opened, not
        // `control_pool`.
        let config = tenant_config_repo::load(&tenant_pool.pool)
            .await?
            .ok_or(RegistryError::NotConfigured(tenant_id))?;

        let pepper = ensure_tenant_pepper(control_pool, keystore, &tenant).await?;
        let dek_cache = KeyCache::new(
            NonZeroUsize::new(DEK_CACHE_CAPACITY)
                .expect("DEK_CACHE_CAPACITY is nonzero"),
            DEK_CACHE_TTL,
        );

        let kill_switches = Arc::new(KillSwitchCache::new());
        let cancel = Arc::new(Notify::new());
        let poll_pool = tenant_pool.pool.clone();
        let poll_cache = kill_switches.clone();
        let poll_cancel = cancel.clone();
        let poll_task = tokio::spawn(async move {
            run_refresh_loop(
                poll_cache,
                poll_pool,
                None,
                KILL_SWITCH_POLL_INTERVAL,
                |_| {},
                Some(poll_cancel),
            )
            .await;
        });

        let context = Arc::new(TenantContext {
            tenant,
            pool: tenant_pool.pool,
            config,
            pepper,
            dek_cache,
            kill_switches,
        });
        contexts.insert(
            tenant_id,
            RegistryEntry {
                context: context.clone(),
                cancel,
                poll_task,
                last_access: StdMutex::new(Instant::now()),
            },
        );
        Ok(context)
    }

    /// Removes every entry idle past `idle_ttl`, signalling its poll loop to
    /// stop and waiting for it to actually exit before returning — proof
    /// the pool is no longer being polled, not just orphaned (T-031).
    pub async fn evict_idle(&self, idle_ttl: Duration) {
        let removed: Vec<RegistryEntry> = {
            let mut contexts = self.contexts.write().await;
            let expired: Vec<Uuid> = contexts
                .iter()
                .filter(|(_, entry)| {
                    entry
                        .last_access
                        .lock()
                        .expect("TenantRegistry last_access mutex poisoned")
                        .elapsed()
                        >= idle_ttl
                })
                .map(|(id, _)| *id)
                .collect();
            expired
                .into_iter()
                .filter_map(|id| contexts.remove(&id))
                .collect()
        };
        for entry in removed {
            entry.cancel.notify_one();
            let _ = entry.poll_task.await;
        }
    }

    /// Spawns the periodic idle-TTL sweep once. Never called from `new()` —
    /// `KillSwitchCache::new()` doesn't auto-spawn its own poll loop either;
    /// callers (this module, `messgr-dispatcher`) always spawn it explicitly,
    /// and existing test fixtures construct a `TenantRegistry` without
    /// wanting a background sweep running on production timings (T-031).
    pub fn start_eviction_sweep(
        self: &Arc<Self>,
        idle_ttl: Duration,
        sweep_interval: Duration,
    ) {
        let registry = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(sweep_interval).await;
                registry.evict_idle(idle_ttl).await;
            }
        });
    }
}

impl Default for TenantRegistry {
    fn default() -> Self {
        Self::new()
    }
}

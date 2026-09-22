//! `tenant.auth_enabled`'s first reader (decision 8, closing Still Open #9
//! for build-order step 17). One process-wide cache, refreshed every 5
//! seconds against the control database directly -- not a per-tenant poll
//! task and not `kill_switch::cache::KillSwitchCache` (shape mirrored, not
//! reused: `auth_enabled` is one query against `control_pool`, with no
//! per-tenant database to open). A row missing from the last successful
//! refresh, or a refresh that itself failed, reads as enabled -- the
//! migration's own fail-open mandate ("a control-database outage must
//! never silently disable customer login").

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use tokio::sync::RwLock;
use uuid::Uuid;

pub struct AuthEnabledCache {
    flags: RwLock<HashMap<Uuid, bool>>,
}

impl AuthEnabledCache {
    pub fn new() -> Self {
        Self {
            flags: RwLock::new(HashMap::new()),
        }
    }

    /// Re-reads every tenant's flag and swaps it in -- built outside the
    /// lock first, like `KillSwitchCache::refresh`, so a failed query never
    /// touches (and therefore never clears) the previous snapshot.
    pub async fn refresh(&self, control_pool: &PgPool) -> Result<(), sqlx::Error> {
        let rows: Vec<(Uuid, bool)> =
            sqlx::query_as("SELECT id, auth_enabled FROM tenant")
                .fetch_all(control_pool)
                .await?;
        let fresh: HashMap<Uuid, bool> = rows.into_iter().collect();
        *self.flags.write().await = fresh;
        Ok(())
    }

    /// `true` for a missing key -- never-yet-refreshed or a newly
    /// provisioned tenant not seen yet -- fail open (decision 8).
    pub async fn is_enabled(&self, tenant_id: Uuid) -> bool {
        self.flags
            .read()
            .await
            .get(&tenant_id)
            .copied()
            .unwrap_or(true)
    }
}

impl Default for AuthEnabledCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Refreshes forever on `poll_interval` -- no `LISTEN`, this is a
/// control-DB poll, not a per-tenant-DB one (decision 8). A refresh error
/// is logged and leaves the previous snapshot in place.
pub async fn run_refresh_loop(
    cache: Arc<AuthEnabledCache>,
    control_pool: PgPool,
    poll_interval: Duration,
) {
    loop {
        if let Err(err) = cache.refresh(&control_pool).await {
            tracing::error!(%err, "sms-sender: auth_enabled cache refresh failed");
        }
        tokio::time::sleep(poll_interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unseen_tenant_reads_as_enabled() {
        let cache = AuthEnabledCache::new();
        assert!(cache.is_enabled(Uuid::new_v4()).await);
    }
}

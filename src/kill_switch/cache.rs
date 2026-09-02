//! In-process active-kill-switch state (DESIGN.md §5.2, T-016), shared by
//! `messgr-dispatcher` and `messgr-ingest`. The dispatcher checks this
//! *before* claiming — never lease-then-block, which busy-waits for the life
//! of the switch (DESIGN.md decision 28) — and refreshes it via a dedicated
//! `LISTEN kill_switch` connection with a 30-second poll fallback.
//! `messgr-ingest` connects through PgBouncer transaction mode, where
//! `LISTEN` never fires (§2.3), so it refreshes on a plain poll only.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use tokio::sync::RwLock;
use uuid::Uuid;

use super::model::{KillSwitch, scope};
use super::repo;

/// What `KillSwitchCache::refresh` observed changed since its previous
/// snapshot. `newly_engaged` drives the discard-at-engage action;
/// `released` drives the release-drain ramp — both dispatcher-only
/// reactions. `messgr-ingest` calls `refresh` too (to keep its own
/// blocking checks current) but has no use for either list.
#[derive(Debug, Default)]
pub struct RefreshDelta {
    pub newly_engaged: Vec<KillSwitch>,
    pub released: Vec<KillSwitch>,
}

pub struct KillSwitchCache {
    active: RwLock<HashMap<Uuid, KillSwitch>>,
}

impl KillSwitchCache {
    pub fn new() -> Self {
        Self {
            active: RwLock::new(HashMap::new()),
        }
    }

    /// Re-reads the active set and swaps it in, returning what changed.
    /// Rows are never deleted (DESIGN.md §5.2 — engage/release only ever
    /// update `released_at`), so an id present before and absent now was
    /// released, never dropped for any other reason.
    pub async fn refresh(&self, pool: &PgPool) -> Result<RefreshDelta, sqlx::Error> {
        let fresh = repo::list_active(pool).await?;
        let fresh_ids: HashSet<Uuid> = fresh.iter().map(|s| s.id).collect();

        let mut guard = self.active.write().await;
        let released: Vec<KillSwitch> = guard
            .values()
            .filter(|s| !fresh_ids.contains(&s.id))
            .cloned()
            .collect();
        let newly_engaged: Vec<KillSwitch> = fresh
            .iter()
            .filter(|s| !guard.contains_key(&s.id))
            .cloned()
            .collect();

        *guard = fresh.into_iter().map(|s| (s.id, s)).collect();

        Ok(RefreshDelta {
            newly_engaged,
            released,
        })
    }

    /// The scope of whichever currently-active switch blocks this
    /// `(channel, producer_id, campaign_id)` tuple, if any —
    /// `messgr-ingest`'s per-request check.
    pub async fn blocking_scope(
        &self,
        channel: &str,
        producer_id: Uuid,
        campaign_id: Option<&str>,
    ) -> Option<String> {
        let guard = self.active.read().await;
        guard
            .values()
            .find(|s| s.matches(channel, producer_id, campaign_id))
            .map(|s| s.scope.clone())
    }

    /// A snapshot of every currently-engaged switch — the dispatcher folds
    /// this together with its own locally-tracked draining scopes (released
    /// switches whose backlog hasn't finished ramping yet) to build one
    /// claim-exclusion set; see `exclusion_for_channel`.
    pub async fn active_snapshot(&self) -> Vec<KillSwitch> {
        self.active.read().await.values().cloned().collect()
    }
}

impl Default for KillSwitchCache {
    fn default() -> Self {
        Self::new()
    }
}

/// What `dispatcher::repo::claim` must exclude from its candidate set for
/// one channel (DESIGN.md §5.2's "excludes matching channels/producers/
/// campaigns from the claim query's candidate set entirely").
#[derive(Debug, Default, Clone)]
pub struct ChannelExclusion {
    /// A `global` switch, or a `channel`-scope switch matching this channel
    /// — nothing on this channel is claimable at all.
    pub blocked_entirely: bool,
    pub blocked_producer_ids: Vec<Uuid>,
    pub blocked_campaign_ids: Vec<String>,
}

/// Builds one channel's exclusion from any iterator of switches — the
/// dispatcher calls this once with the cache's own engaged set, and again
/// (or chained) with its locally-tracked draining set, since both must
/// exclude a row from the *normal* claim loop; only the release-drain task
/// itself is allowed to claim a draining scope's rows.
pub fn exclusion_for_channel<'a>(
    switches: impl Iterator<Item = &'a KillSwitch>,
    channel: &str,
) -> ChannelExclusion {
    let mut result = ChannelExclusion::default();
    for s in switches {
        match s.scope.as_str() {
            scope::GLOBAL => result.blocked_entirely = true,
            scope::CHANNEL if s.scope_key.as_deref() == Some(channel) => {
                result.blocked_entirely = true;
            }
            scope::PRODUCER => {
                if let Some(id) = s.producer_id() {
                    result.blocked_producer_ids.push(id);
                }
            }
            scope::PRODUCER_CHANNEL => {
                if let Some((id, ch)) = s.producer_channel_parts()
                    && ch == channel
                {
                    result.blocked_producer_ids.push(id);
                }
            }
            scope::CAMPAIGN => {
                if let Some(campaign_id) = &s.scope_key {
                    result.blocked_campaign_ids.push(campaign_id.clone());
                }
            }
            _ => {}
        }
    }
    result
}

/// Refreshes forever on `poll_interval`, optionally woken early by
/// `listener` (already subscribed to the `kill_switch` channel — the
/// dispatcher's direct-connection fast path; `messgr-ingest` passes `None`,
/// per this module's own doc comment). `on_delta` receives every refresh's
/// `RefreshDelta`, including empty ones, so the dispatcher's caller can
/// react to engages/releases; `messgr-ingest` passes a no-op closure.
pub async fn run_refresh_loop(
    cache: Arc<KillSwitchCache>,
    pool: PgPool,
    mut listener: Option<sqlx::postgres::PgListener>,
    poll_interval: Duration,
    mut on_delta: impl FnMut(RefreshDelta),
) {
    loop {
        match cache.refresh(&pool).await {
            Ok(delta) => on_delta(delta),
            Err(err) => tracing::error!(%err, "kill_switch cache refresh failed"),
        }

        match &mut listener {
            Some(listener) => {
                tokio::select! {
                    _ = listener.recv() => {}
                    _ = tokio::time::sleep(poll_interval) => {}
                }
            }
            None => tokio::time::sleep(poll_interval).await,
        }
    }
}

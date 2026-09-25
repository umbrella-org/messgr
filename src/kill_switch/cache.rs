//! In-process active-kill-switch state (DESIGN.md §5.2, T-016), shared by
//! `messgr-dispatcher` and `messgr-ingest`. The dispatcher checks this
//! *before* claiming — never lease-then-block, which busy-waits for the life
//! of the switch (DESIGN.md decision 28) — and refreshes it via a dedicated
//! `LISTEN kill_switch` connection with a 30-second poll fallback.
//! `messgr-ingest` connects through PgBouncer transaction mode, where
//! `LISTEN` never fires (§2.3), so it refreshes on a plain poll only.
//! Both also merge in the control database's `platform_kill_switch` rows
//! for their tenant (T-058), so the platform tier rides the same cache.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use tokio::sync::{Notify, RwLock};
use uuid::Uuid;

use super::model::{KillSwitch, scope};
use super::repo;
use crate::platform_kill_switch::repo as platform_repo;

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
    active: RwLock<ActiveSet>,
}

/// Engaged switches keyed by id — the tenant's own `kill_switch` rows plus,
/// when a refresh was given a platform source, each applicable
/// `platform_kill_switch` row as its synthetic `global`/`hold` form (T-058
/// decision 1). `platform_ids` remembers which entries are platform-tier,
/// so `blocking_scope` can report them as such instead of as `global`.
#[derive(Default)]
struct ActiveSet {
    switches: HashMap<Uuid, KillSwitch>,
    platform_ids: HashSet<Uuid>,
}

/// `blocking_scope`'s answer for a platform-tier switch (T-058) — distinct
/// from every tenant scope, so `messgr-ingest`'s `503` tells a producer
/// team the provider suspended the tenant, not that its own ops team
/// engaged a `global` switch.
pub const PLATFORM_SCOPE: &str = "platform";

impl KillSwitchCache {
    pub fn new() -> Self {
        Self {
            active: RwLock::new(ActiveSet::default()),
        }
    }

    /// Re-reads the tenant's own active set and swaps it in, returning what
    /// changed — `refresh_with_platform` without a platform source, for
    /// callers with no control-database access.
    pub async fn refresh(&self, pool: &PgPool) -> Result<RefreshDelta, sqlx::Error> {
        self.refresh_with_platform(pool, None).await
    }

    /// Re-reads the active set and swaps it in, returning what changed.
    /// Rows are never deleted (DESIGN.md §5.2 — engage/release only ever
    /// update `released_at`), so an id present before and absent now was
    /// released, never dropped for any other reason.
    ///
    /// `platform`, when given, is `(control_pool, tenant_id)`: the
    /// `platform_kill_switch` rows blocking this tenant are read in the same
    /// refresh and merged in (T-058 decision 3), so the fan-out `NOTIFY`
    /// that wakes the dispatcher's loop wakes it into a read that actually
    /// sees them. If that control-database read fails, the previous
    /// platform entries are carried over unchanged while the tenant's own
    /// rows still refresh: a control-database blip must never look like a
    /// platform switch releasing, and must never stop the tenant's own
    /// switches taking effect either. A tenant-database read failure fails
    /// the whole refresh, as before T-058.
    pub async fn refresh_with_platform(
        &self,
        pool: &PgPool,
        platform: Option<(&PgPool, Uuid)>,
    ) -> Result<RefreshDelta, sqlx::Error> {
        let mut fresh = repo::list_active(pool).await?;
        let platform_rows = match platform {
            None => Some(Vec::new()),
            Some((control_pool, tenant_id)) => {
                match platform_repo::list_active_for_tenant(control_pool, tenant_id)
                    .await
                {
                    Ok(rows) => Some(rows),
                    Err(err) => {
                        tracing::error!(
                            %err,
                            %tenant_id,
                            "platform_kill_switch read failed; keeping the last-known platform switches"
                        );
                        None
                    }
                }
            }
        };

        let mut guard = self.active.write().await;
        let platform_ids: HashSet<Uuid> = match platform_rows {
            Some(rows) => rows
                .into_iter()
                .map(|row| {
                    fresh.push(row.as_kill_switch());
                    row.id
                })
                .collect(),
            None => {
                fresh.extend(
                    guard
                        .platform_ids
                        .iter()
                        .filter_map(|id| guard.switches.get(id).cloned()),
                );
                guard.platform_ids.clone()
            }
        };
        let fresh_ids: HashSet<Uuid> = fresh.iter().map(|s| s.id).collect();

        let released: Vec<KillSwitch> = guard
            .switches
            .values()
            .filter(|s| !fresh_ids.contains(&s.id))
            .cloned()
            .collect();
        let newly_engaged: Vec<KillSwitch> = fresh
            .iter()
            .filter(|s| !guard.switches.contains_key(&s.id))
            .cloned()
            .collect();

        *guard = ActiveSet {
            switches: fresh.into_iter().map(|s| (s.id, s)).collect(),
            platform_ids,
        };

        Ok(RefreshDelta {
            newly_engaged,
            released,
        })
    }

    /// The scope of whichever currently-active switch blocks this
    /// `(channel, producer_id, campaign_id)` tuple, if any —
    /// `messgr-ingest`'s per-request check. A platform-tier switch blocks
    /// everything and is checked first, reported as `PLATFORM_SCOPE`.
    pub async fn blocking_scope(
        &self,
        channel: &str,
        producer_id: Uuid,
        campaign_id: Option<&str>,
    ) -> Option<String> {
        let guard = self.active.read().await;
        if !guard.platform_ids.is_empty() {
            return Some(PLATFORM_SCOPE.to_string());
        }
        guard
            .switches
            .values()
            .find(|s| s.matches(channel, producer_id, campaign_id))
            .map(|s| s.scope.clone())
    }

    /// A snapshot of every currently-engaged switch — the dispatcher folds
    /// this together with its own locally-tracked draining scopes (released
    /// switches whose backlog hasn't finished ramping yet) to build one
    /// claim-exclusion set; see `exclusion_for_channel`.
    pub async fn active_snapshot(&self) -> Vec<KillSwitch> {
        self.active
            .read()
            .await
            .switches
            .values()
            .cloned()
            .collect()
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
/// `platform`, if given, is `(control_pool, tenant_id)` and is passed to
/// every `refresh_with_platform` (T-058): both `messgr-dispatcher` and
/// `messgr-ingest` pass it, so a platform switch reaches them on the same
/// tick as a tenant switch.
/// `cancel`, if given, stops the loop as soon as it's notified — this is
/// what lets `TenantRegistry` (T-031) actually tear down a per-tenant poll
/// task on eviction rather than merely dropping its handle; `messgr-dispatcher`
/// passes `None`, since a dispatcher is one-tenant-per-process for its own
/// lifetime and never evicts.
pub async fn run_refresh_loop(
    cache: Arc<KillSwitchCache>,
    pool: PgPool,
    mut listener: Option<sqlx::postgres::PgListener>,
    poll_interval: Duration,
    mut on_delta: impl FnMut(RefreshDelta),
    cancel: Option<Arc<Notify>>,
    platform: Option<(PgPool, Uuid)>,
) {
    loop {
        let platform_source = platform.as_ref().map(|(p, id)| (p, *id));
        match cache.refresh_with_platform(&pool, platform_source).await {
            Ok(delta) => on_delta(delta),
            Err(err) => tracing::error!(%err, "kill_switch cache refresh failed"),
        }

        let sleep = tokio::time::sleep(poll_interval);
        let cancelled = async {
            match &cancel {
                Some(notify) => notify.notified().await,
                None => std::future::pending().await,
            }
        };

        match &mut listener {
            Some(listener) => tokio::select! {
                _ = listener.recv() => {}
                _ = sleep => {}
                _ = cancelled => break,
            },
            None => tokio::select! {
                _ = sleep => {}
                _ = cancelled => break,
            },
        }
    }
}

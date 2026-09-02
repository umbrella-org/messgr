//! Kill-switch release-drain and engage-discard tasks (DESIGN.md §5.2,
//! T-016 decisions 4 and 11). Both are one-shot: they run until their
//! scope's backlog is empty, then stop.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

use crate::kill_switch::model::KillSwitch;

use super::repo;
use super::worker::{DispatcherContext, LEASE_DURATION, process_one};

/// Drains a released `hold` switch's backlog at `release_rate` rows per
/// second per channel, rather than letting the whole backlog become
/// claimable the instant it releases (DESIGN.md §5.2's ramp). Spawned once
/// per channel this dispatcher process runs — each drain task claims only
/// its own channel's matching rows (`repo::claim_for_scope`'s `channel`
/// pin), since sending needs that channel's own `Sender`. A row whose
/// `expires_at` has passed is written `expired` instead of sent (§6.2 — the
/// exact correction this ticket's Description cites). The caller is
/// responsible for removing `kill_switch.id` from `draining` once every
/// channel's task has finished; this function only drains its own channel.
pub async fn drain_released_scope(
    ctx: Arc<DispatcherContext>,
    channel: String,
    kill_switch: KillSwitch,
    release_rate: i64,
) {
    loop {
        let leased_until = Utc::now() + LEASE_DURATION;
        let batch = match repo::claim_for_scope(
            &ctx.pool,
            Some(&channel),
            &kill_switch,
            release_rate,
            leased_until,
        )
        .await
        {
            Ok(rows) => rows,
            Err(err) => {
                tracing::error!(
                    kill_switch_id = %kill_switch.id,
                    %channel,
                    %err,
                    "kill-switch drain: claim failed"
                );
                return;
            }
        };

        if batch.is_empty() {
            return;
        }

        let now = Utc::now();
        for row in batch {
            if row.expires_at.is_some_and(|expires_at| expires_at <= now) {
                if let Err(err) = repo::write_terminal(
                    &ctx.pool,
                    row.created_at,
                    row.comms_request_id,
                    row.customer_id,
                    "expired",
                    None,
                    None,
                    "expired",
                )
                .await
                {
                    tracing::error!(
                        comms_request_id = %row.comms_request_id,
                        %err,
                        "kill-switch drain: writing expired terminal failed"
                    );
                }
            } else {
                process_one(&ctx, row).await;
            }
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Runs every channel's `drain_released_scope` for one released switch to
/// completion, then drops it from `draining` — the exclusion
/// (`DispatcherContext::claim_exclusion`) stops covering this scope only
/// once every channel has finished, so a `global`/`producer`/`campaign`
/// switch's backlog on a slower channel can't leak into the normal claim
/// loop while a faster channel's drain has already finished.
pub async fn run_release_drain(
    contexts: HashMap<String, Arc<DispatcherContext>>,
    draining: Arc<RwLock<HashMap<Uuid, KillSwitch>>>,
    kill_switch: KillSwitch,
    release_rate: i64,
) {
    let mut handles = Vec::new();
    for (channel, ctx) in &contexts {
        handles.push(tokio::spawn(drain_released_scope(
            ctx.clone(),
            channel.clone(),
            kill_switch.clone(),
            release_rate,
        )));
    }
    for handle in handles {
        let _ = handle.await;
    }

    draining
        .write()
        .expect("draining lock poisoned")
        .remove(&kill_switch.id);
}

/// Immediately terminal-writes (`discarded`) every row an engaged `discard`
/// switch matches, across every channel at once (`channel: None` —
/// discarding never sends, so it needs no channel-specific `Sender`,
/// unlike `drain_released_scope`). DESIGN.md §5.2: "Discard exists because
/// sometimes [incidents don't end in resume] — a six-hour-old flash-sale
/// blast firing after the sale ended is worse than never sending it."
pub async fn discard_engaged_scope(pool: PgPool, kill_switch: KillSwitch, batch_size: i64) {
    loop {
        let leased_until = Utc::now() + LEASE_DURATION;
        let batch =
            match repo::claim_for_scope(&pool, None, &kill_switch, batch_size, leased_until)
                .await
            {
                Ok(rows) => rows,
                Err(err) => {
                    tracing::error!(
                        kill_switch_id = %kill_switch.id,
                        %err,
                        "kill-switch discard: claim failed"
                    );
                    return;
                }
            };

        if batch.is_empty() {
            return;
        }

        for row in batch {
            if let Err(err) = repo::write_terminal(
                &pool,
                row.created_at,
                row.comms_request_id,
                row.customer_id,
                "discarded",
                None,
                None,
                "discarded",
            )
            .await
            {
                tracing::error!(
                    comms_request_id = %row.comms_request_id,
                    %err,
                    "kill-switch discard: writing discarded terminal failed"
                );
            }
        }
    }
}

//! Kill-switch release-drain and engage-discard tasks (DESIGN.md §5.2,
//! T-016 decisions 4 and 11). Both are one-shot: they run until their
//! scope's backlog is empty, then stop.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

use crate::kill_switch::cache::{ChannelExclusion, exclusion_for_channel};
use crate::kill_switch::model::KillSwitch;

use super::model::ClaimedOutbox;
use super::repo;
use super::worker::{DispatcherContext, LEASE_DURATION, process_one};

/// How long a drain/discard task waits before retrying a failed `claim_for_
/// scope` or `write_terminal` call (F1 rework). Neither task may treat a
/// transient DB error as "the backlog is empty" — only an actual empty
/// claim means that — so both retry instead of returning on error; see
/// this module's own doc comment for why silently giving up defeats the
/// switch's own guarantee.
const RETRY_DELAY: Duration = Duration::from_secs(1);

/// How many times a single row's terminal write is retried before this task
/// gives up on *that row specifically* and moves on (F2 rework). Unlike
/// `claim_for_scope`'s own retry (unbounded — a failure there claims nothing,
/// so it blocks no other row), a per-row `write_terminal` retry sits inside
/// the batch's own `for` loop: an unbounded version of it stalls every row
/// behind this one, every later `claim_for_scope` call for this scope, and,
/// for `run_release_drain`, the whole switch's `draining` entry forever,
/// since the channel's task never returns. Five attempts (five seconds at
/// `RETRY_DELAY`) rides out an ordinary transient blip while still bounding
/// how long one bad row can hold up everything after it.
const MAX_WRITE_TERMINAL_ATTEMPTS: u32 = 5;

/// Writes a claimed row's `expired` terminal state, retrying on failure up
/// to `MAX_WRITE_TERMINAL_ATTEMPTS` times (F1/F2 rework) before giving up on
/// this row and returning anyway, so one permanently-failing row cannot
/// block every row after it. A row given up on this way stays leased until
/// its lease naturally expires, at which point it becomes an ordinary
/// outbox row again — the same fate any other row's mid-dispatch failure
/// already has elsewhere in this codebase (`worker::process_one`).
async fn write_expired(ctx: &DispatcherContext, row: &ClaimedOutbox) {
    for attempt in 1..=MAX_WRITE_TERMINAL_ATTEMPTS {
        match repo::write_terminal(
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
            Ok(()) => return,
            Err(err) if attempt < MAX_WRITE_TERMINAL_ATTEMPTS => {
                tracing::error!(
                    comms_request_id = %row.comms_request_id,
                    %err,
                    attempt,
                    "kill-switch drain: writing expired terminal failed, retrying"
                );
                tokio::time::sleep(RETRY_DELAY).await;
            }
            Err(err) => {
                tracing::error!(
                    comms_request_id = %row.comms_request_id,
                    %err,
                    attempts = MAX_WRITE_TERMINAL_ATTEMPTS,
                    "kill-switch drain: writing expired terminal failed repeatedly, \
                     giving up on this row — it remains leased, unresolved, and needs \
                     operator investigation; the rest of this scope's backlog continues"
                );
            }
        }
    }
}

/// Drains a released `hold` switch's backlog at `release_rate` rows per
/// second per channel, rather than letting the whole backlog become
/// claimable the instant it releases (DESIGN.md §5.2's ramp). Spawned once
/// per channel this dispatcher process runs — each drain task claims only
/// its own channel's matching rows (`repo::claim_for_scope`'s `channel`
/// pin), since sending needs that channel's own `Sender`. A row whose
/// `expires_at` has passed is written `expired` instead of sent (§6.2 — the
/// exact correction this ticket's Description cites). The caller is
/// responsible for removing `kill_switch.id` from `draining` once every
/// channel's task has finished; this function only drains its own channel,
/// and only ever returns once an actual claim comes back empty (F1 rework)
/// — a `claim_for_scope` error retries rather than ending the task, since
/// `run_release_drain` cannot tell "finished" from "gave up" apart and
/// would otherwise lift the exclusion on whatever backlog remains,
/// dispatching it at full, unthrottled speed (the exact stampede DESIGN.md
/// decision 12 exists to prevent).
pub async fn drain_released_scope(
    ctx: Arc<DispatcherContext>,
    channel: String,
    kill_switch: KillSwitch,
    release_rate: i64,
) {
    loop {
        let leased_until = Utc::now() + LEASE_DURATION;
        // Rebuilt every batch from the *engaged* set only (T-058 decision
        // 5): a switch engaged mid-drain stops this drain touching its rows
        // at once, and other draining scopes are deliberately not included
        // -- two overlapping drains must not starve each other.
        let engaged = ctx.kill_switches.active_snapshot().await;
        let exclusion = exclusion_for_channel(engaged.iter(), &channel);
        let batch = match repo::claim_for_scope(
            &ctx.pool,
            Some(&channel),
            &kill_switch,
            release_rate,
            leased_until,
            &exclusion,
        )
        .await
        {
            Ok(rows) => rows,
            Err(err) => {
                tracing::error!(
                    kill_switch_id = %kill_switch.id,
                    %channel,
                    %err,
                    "kill-switch drain: claim failed, retrying — the backlog is not \
                     considered drained until this succeeds"
                );
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
        };

        if batch.is_empty() {
            return;
        }

        let now = Utc::now();
        for row in batch {
            if row.expires_at.is_some_and(|expires_at| expires_at <= now) {
                write_expired(&ctx, &row).await;
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

/// Writes a claimed row's `discarded` terminal state, retrying on failure up
/// to `MAX_WRITE_TERMINAL_ATTEMPTS` times (F1/F2 rework) — same reasoning as
/// `write_expired`: bounded so one row cannot block every row after it in
/// the same batch, or the scope's later batches. A row given up on this way
/// stays engaged-and-excluded (held) for as long as the switch stays
/// engaged; only a later release without this row ever having been swept
/// can still dispatch it for real, same as any row this task never reached
/// at all.
async fn write_discarded(pool: &PgPool, row: &ClaimedOutbox) {
    for attempt in 1..=MAX_WRITE_TERMINAL_ATTEMPTS {
        match repo::write_terminal(
            pool,
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
            Ok(()) => return,
            Err(err) if attempt < MAX_WRITE_TERMINAL_ATTEMPTS => {
                tracing::error!(
                    comms_request_id = %row.comms_request_id,
                    %err,
                    attempt,
                    "kill-switch discard: writing discarded terminal failed, retrying"
                );
                tokio::time::sleep(RETRY_DELAY).await;
            }
            Err(err) => {
                tracing::error!(
                    comms_request_id = %row.comms_request_id,
                    %err,
                    attempts = MAX_WRITE_TERMINAL_ATTEMPTS,
                    "kill-switch discard: writing discarded terminal failed repeatedly, \
                     giving up on this row — it remains leased, unresolved, and needs \
                     operator investigation; the rest of this scope's backlog continues"
                );
            }
        }
    }
}

/// Immediately terminal-writes (`discarded`) every row an engaged `discard`
/// switch matches, across every channel at once (`channel: None` —
/// discarding never sends, so it needs no channel-specific `Sender`,
/// unlike `drain_released_scope`). DESIGN.md §5.2: "Discard exists because
/// sometimes [incidents don't end in resume] — a six-hour-old flash-sale
/// blast firing after the sale ended is worse than never sending it."
/// Only ever returns once an actual claim comes back empty (F1 rework) — a
/// `claim_for_scope` error retries rather than ending the task: this
/// scope's rows stay excluded from the normal claim loop only while it is
/// engaged, so a row this task gives up on gets dispatched and sent for
/// real if the switch is later released, the opposite of what engaging
/// `on_queued = 'discard'` asked for.
pub async fn discard_engaged_scope(
    pool: PgPool,
    kill_switch: KillSwitch,
    batch_size: i64,
) {
    loop {
        let leased_until = Utc::now() + LEASE_DURATION;
        let batch = match repo::claim_for_scope(
            &pool,
            None,
            &kill_switch,
            batch_size,
            leased_until,
            &ChannelExclusion::default(),
        )
        .await
        {
            Ok(rows) => rows,
            Err(err) => {
                tracing::error!(
                    kill_switch_id = %kill_switch.id,
                    %err,
                    "kill-switch discard: claim failed, retrying — this scope's backlog \
                     is not considered discarded until this succeeds"
                );
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
        };

        if batch.is_empty() {
            return;
        }

        for row in batch {
            write_discarded(&pool, &row).await;
        }
    }
}

//! The per-channel claim loop (DESIGN.md §4.2, §9, T-013).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration as StdDuration;

use chrono::{Duration as ChronoDuration, Utc};
use rand::Rng;
use sqlx::PgPool;
use uuid::Uuid;

use crate::customer_dek::lifecycle::{self, CustomerDekError};
use crate::encryption::{self, EncryptionError};
use crate::key_cache::KeyCache;
use crate::keystore::KeyStore;
use crate::kill_switch::cache::{self, ChannelExclusion, KillSwitchCache};
use crate::kill_switch::model::KillSwitch;
use crate::sender::{Sender, SenderError};

use super::model::ClaimedOutbox;
use super::repo;

pub const CLAIM_BATCH_SIZE: i64 = 20;
pub const LEASE_DURATION: ChronoDuration = ChronoDuration::minutes(2);
const POLL_INTERVAL: StdDuration = StdDuration::from_secs(1);
/// How long `run_channel_loop` waits before retrying a failed `repo::claim`
/// call (T-021 decision 5) — mirrors `drain.rs`'s own `RETRY_DELAY`
/// precedent for `claim_for_scope` errors (not shared: that constant is
/// private to that module). A claim failure blocks nothing but this
/// channel's own next iteration, so the retry is unbounded.
const CLAIM_RETRY_DELAY: StdDuration = StdDuration::from_secs(1);

/// A retryable send failure is terminal-failed instead of rescheduled once
/// `outbox.attempts` reaches this many (T-021 decision 2) — `attempts` is
/// already bumped by `repo::claim`'s own `UPDATE` before `try_process` runs,
/// so this is also the count of attempts actually made.
const MAX_SEND_ATTEMPTS: i16 = 8;
/// Backoff base, multiplier, and cap for a retryable send failure (T-021
/// decision 2) — DESIGN.md names "exponential backoff and jitter" (§2.4
/// step 6) but no numbers; confirmed with the user during refinement.
const BACKOFF_BASE: ChronoDuration = ChronoDuration::seconds(30);
const BACKOFF_MULTIPLIER: i64 = 2;
const BACKOFF_CAP: ChronoDuration = ChronoDuration::minutes(30);

/// Everything one channel's claim loop needs, opened once at startup and
/// shared across every row it processes. `kill_switches`/`draining` are
/// shared across every channel this dispatcher process runs (DESIGN.md
/// §5.2's scopes — `global`/`producer`/`campaign` aren't channel-specific),
/// not opened per channel like the rest of this struct.
pub struct DispatcherContext {
    pub pool: PgPool,
    pub keystore: Arc<dyn KeyStore>,
    pub cache: Arc<KeyCache>,
    pub mount: String,
    pub sender: Arc<dyn Sender>,
    pub kill_switches: Arc<KillSwitchCache>,
    /// Released switches whose backlog hasn't finished the release-drain
    /// ramp yet (T-016 decision 4). Kept separate from `kill_switches`
    /// (which only ever holds *engaged* switches) because a released switch
    /// must still be excluded from the normal claim loop until its own
    /// drain task empties it — only that task is allowed to claim its rows.
    /// A plain `std::sync::RwLock`, not `tokio::sync::RwLock`: the refresh
    /// loop's `on_delta` callback (`kill_switch::cache::run_refresh_loop`)
    /// is synchronous by design, so this insert can happen in the same
    /// tick that removes the switch from `kill_switches` — no `.await`
    /// between the two means no window where a released scope is excluded
    /// by neither map. Never held across an `.await` point.
    pub draining: Arc<RwLock<HashMap<Uuid, KillSwitch>>>,
}

impl DispatcherContext {
    /// The exclusion `repo::claim` must apply for `channel`: the union of
    /// currently-engaged switches and scopes still draining after release.
    pub async fn claim_exclusion(&self, channel: &str) -> ChannelExclusion {
        let mut switches = self.kill_switches.active_snapshot().await;
        switches.extend(
            self.draining
                .read()
                .expect("draining lock poisoned")
                .values()
                .cloned(),
        );
        cache::exclusion_for_channel(switches.iter(), channel)
    }
}

#[derive(Debug)]
pub enum DispatchError {
    Database(sqlx::Error),
    Dek(CustomerDekError),
    Encryption(EncryptionError),
    InvalidUtf8(std::string::FromUtf8Error),
    /// The outbox row's own ledger row is missing — should be unreachable,
    /// since T-011 writes both in the same transaction.
    MissingRequest(Uuid),
    /// `payload_ciphertext` was `NULL` — should be unreachable, since
    /// `class = "auth"` (the one class allowed a null payload) never
    /// reaches the outbox (T-011 decision 3).
    MissingPayload(Uuid),
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(err) => write!(f, "dispatcher database error: {err}"),
            Self::Dek(err) => write!(f, "dispatcher DEK error: {err}"),
            Self::Encryption(err) => write!(f, "dispatcher decryption error: {err}"),
            Self::InvalidUtf8(err) => {
                write!(f, "dispatcher decrypted invalid utf8: {err}")
            }
            Self::MissingRequest(id) => {
                write!(f, "no comms_request row for outbox row {id}")
            }
            Self::MissingPayload(id) => {
                write!(f, "comms_request {id} has no payload_ciphertext")
            }
        }
    }
}

impl std::error::Error for DispatchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(err) => Some(err),
            Self::Dek(err) => Some(err),
            Self::Encryption(err) => Some(err),
            Self::InvalidUtf8(err) => Some(err),
            Self::MissingRequest(_) | Self::MissingPayload(_) => None,
        }
    }
}

impl From<sqlx::Error> for DispatchError {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err)
    }
}

impl From<CustomerDekError> for DispatchError {
    fn from(err: CustomerDekError) -> Self {
        Self::Dek(err)
    }
}

impl From<EncryptionError> for DispatchError {
    fn from(err: EncryptionError) -> Self {
        Self::Encryption(err)
    }
}

impl From<std::string::FromUtf8Error> for DispatchError {
    fn from(err: std::string::FromUtf8Error) -> Self {
        Self::InvalidUtf8(err)
    }
}

/// Exponential backoff with jitter for a retryable send failure (T-021
/// decision 2), keyed on `outbox.attempts` at the time of the failure that
/// just happened (1 for the first attempt, ...). Exponent capped at 6 —
/// `BACKOFF_BASE * BACKOFF_MULTIPLIER^6` already exceeds `BACKOFF_CAP`, so
/// growing it further has no effect once `.min(BACKOFF_CAP)` applies.
/// Jitter is uniform 0-50% of the capped delay, added on top, matching
/// §6.1's own jitter style for quiet-hours rescheduling.
fn backoff_delay(attempts: i16) -> ChronoDuration {
    let exponent = attempts.saturating_sub(1).min(6) as u32;
    let delay_ms = BACKOFF_BASE
        .num_milliseconds()
        .saturating_mul(BACKOFF_MULTIPLIER.saturating_pow(exponent))
        .min(BACKOFF_CAP.num_milliseconds());
    let jitter_ms = rand::thread_rng().gen_range(0..=(delay_ms / 2).max(1));
    ChronoDuration::milliseconds(delay_ms + jitter_ms)
}

/// `Some(status)` for a `Provider` rejection, `None` for a transport-level
/// `Http` failure — matches `try_process`'s pre-T-021 terminal-write shape
/// for `provider_status`.
fn provider_status_of(err: &SenderError) -> Option<String> {
    match err {
        SenderError::Provider { status, .. } => Some(status.to_string()),
        SenderError::Http(_) => None,
    }
}

/// `pub` (not private) so integration tests can drive one claimed row's
/// decrypt/send/write-terminal path directly, without standing up a whole
/// `run_channel_loop`.
pub async fn try_process(
    ctx: &DispatcherContext,
    row: &ClaimedOutbox,
) -> Result<(), DispatchError> {
    let ciphertexts =
        repo::load_ciphertexts(&ctx.pool, row.created_at, row.comms_request_id)
            .await?
            .ok_or(DispatchError::MissingRequest(row.comms_request_id))?;

    let dek = lifecycle::get_or_create_dek(
        &ctx.pool,
        ctx.keystore.as_ref(),
        ctx.cache.as_ref(),
        &ctx.mount,
        row.customer_id,
    )
    .await?;

    let aad = row.comms_request_id.as_bytes();
    let destination = String::from_utf8(encryption::decrypt(
        &dek,
        aad,
        &ciphertexts.destination_ciphertext,
    )?)?;
    let payload_ciphertext = ciphertexts
        .payload_ciphertext
        .ok_or(DispatchError::MissingPayload(row.comms_request_id))?;
    let body = String::from_utf8(encryption::decrypt(&dek, aad, &payload_ciphertext)?)?;

    match ctx.sender.send(&destination, &body).await {
        Ok(outcome) => {
            repo::write_terminal(
                &ctx.pool,
                row.created_at,
                row.comms_request_id,
                row.customer_id,
                "sent",
                Some(&outcome.provider_ref),
                Some(&outcome.provider_status),
                "sent",
            )
            .await?;
        }
        Err(err) if err.is_retryable() && row.attempts < MAX_SEND_ATTEMPTS => {
            repo::reschedule_retry(
                &ctx.pool,
                row.comms_request_id,
                Utc::now() + backoff_delay(row.attempts),
            )
            .await?;
        }
        Err(err) => {
            repo::write_terminal(
                &ctx.pool,
                row.created_at,
                row.comms_request_id,
                row.customer_id,
                "failed",
                None,
                provider_status_of(&err).as_deref(),
                "failed",
            )
            .await?;
        }
    }

    Ok(())
}

/// Processes one claimed row, logging rather than propagating on failure —
/// one bad row must not stop the loop from claiming the rest of its batch.
/// `pub(crate)`, not private: the release-drain task (`super::drain`) reuses
/// this exact send-or-fail path for rows it claims itself.
pub(crate) async fn process_one(ctx: &DispatcherContext, row: ClaimedOutbox) {
    if let Err(err) = try_process(ctx, &row).await {
        tracing::error!(
            comms_request_id = %row.comms_request_id,
            %err,
            "dispatcher: failed to process claimed outbox row"
        );
    }
}

/// Runs forever: claims a batch for `channel`, drains it, and — only when a
/// claim comes back empty — waits for either a `LISTEN` wakeup or the 1s
/// poll fallback (DESIGN.md §4.2) before claiming again.
pub async fn run_channel_loop(ctx: Arc<DispatcherContext>, channel: String) {
    let mut listener = sqlx::postgres::PgListener::connect_with(&ctx.pool)
        .await
        .expect("dispatcher: failed to open a dedicated LISTEN connection");
    listener
        .listen(&format!("outbox_{channel}"))
        .await
        .expect("dispatcher: failed to LISTEN on this channel's notify topic");

    loop {
        let leased_until = Utc::now() + LEASE_DURATION;
        let exclusion = ctx.claim_exclusion(&channel).await;
        let claimed = loop {
            match repo::claim(
                &ctx.pool,
                &channel,
                CLAIM_BATCH_SIZE,
                leased_until,
                &exclusion,
            )
            .await
            {
                Ok(rows) => break rows,
                Err(err) => {
                    tracing::error!(
                        %channel,
                        %err,
                        "dispatcher: claim query failed, retrying"
                    );
                    tokio::time::sleep(CLAIM_RETRY_DELAY).await;
                }
            }
        };

        if claimed.is_empty() {
            tokio::select! {
                _ = listener.recv() => {}
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }
            continue;
        }

        for row in claimed {
            process_one(&ctx, row).await;
        }
    }
}

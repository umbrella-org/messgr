//! Claim, ciphertext lookup, and terminal write (DESIGN.md §4.1, §4.2, §4.4,
//! T-013).

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::kill_switch::cache::ChannelExclusion;
use crate::kill_switch::model::{KillSwitch, scope};

use super::model::{ClaimedOutbox, RequestCiphertexts};

/// Leases up to `limit` ready rows for `channel` (DESIGN.md §4.2's own claim
/// query), bumping `attempts` as part of the same `UPDATE` (T-013 decision
/// 10). `leased_until` is computed by the caller (`Utc::now() +
/// LEASE_DURATION`) rather than bound as a Postgres `interval`.
///
/// `exclusion` (DESIGN.md §5.2, T-016 decision 3) is checked *before*
/// claiming, not after: a `global`/`channel`-scope match skips the query
/// entirely, and `producer`/`campaign`-scope matches are excluded from the
/// candidate set via `<> ALL`. This is what makes "held" a real state rather
/// than a busy-wait — a matching row is never leased, gate-blocked, and
/// re-leased on the next tick (DESIGN.md decision 28).
pub async fn claim(
    pool: &PgPool,
    channel: &str,
    limit: i64,
    leased_until: DateTime<Utc>,
    exclusion: &ChannelExclusion,
) -> Result<Vec<ClaimedOutbox>, sqlx::Error> {
    if exclusion.blocked_entirely {
        return Ok(Vec::new());
    }

    sqlx::query_as::<_, ClaimedOutbox>(
        r#"
        UPDATE outbox SET leased_until = $3, attempts = attempts + 1
        WHERE comms_request_id IN (
            SELECT comms_request_id FROM outbox
            WHERE channel = $1 AND next_attempt_at <= now() AND leased_until IS NULL
              AND producer_id <> ALL($4)
              AND (campaign_id IS NULL OR campaign_id <> ALL($5))
            ORDER BY priority, next_attempt_at
            LIMIT $2
            FOR UPDATE SKIP LOCKED
        )
        RETURNING comms_request_id, created_at, channel, class, priority, customer_id,
                  address_id, producer_id, campaign_id, next_attempt_at, expires_at,
                  cancelled_at, attempts, leased_until
        "#,
    )
    .bind(channel)
    .bind(limit)
    .bind(leased_until)
    .bind(&exclusion.blocked_producer_ids)
    .bind(&exclusion.blocked_campaign_ids)
    .fetch_all(pool)
    .await
}

/// The inverse of `claim`'s exclusion: leases up to `limit` rows matching
/// `kill_switch`'s own scope instead of excluding it — the release-drain
/// ramp's (T-016 decision 4) candidate query. `channel`, when `Some`, also
/// pins the result to one channel (the release-drain task needs this,
/// since it must hand each row to that channel's own `Sender`); `None`
/// (the discard-at-engage task, which never sends) matches every channel a
/// channel-agnostic scope (`global`/`producer`/`campaign`) covers.
pub async fn claim_for_scope(
    pool: &PgPool,
    channel: Option<&str>,
    kill_switch: &KillSwitch,
    limit: i64,
    leased_until: DateTime<Utc>,
) -> Result<Vec<ClaimedOutbox>, sqlx::Error> {
    let mut qb = sqlx::QueryBuilder::new("UPDATE outbox SET leased_until = ");
    qb.push_bind(leased_until);
    qb.push(
        ", attempts = attempts + 1 WHERE comms_request_id IN (\
          SELECT comms_request_id FROM outbox \
          WHERE next_attempt_at <= now() AND leased_until IS NULL",
    );

    if let Some(channel) = channel {
        qb.push(" AND channel = ");
        qb.push_bind(channel.to_string());
    }

    match kill_switch.scope.as_str() {
        scope::GLOBAL => {}
        scope::CHANNEL => {
            let Some(switch_channel) = kill_switch.scope_key.as_deref() else {
                return Ok(Vec::new());
            };
            match channel {
                Some(pinned) if pinned != switch_channel => return Ok(Vec::new()),
                Some(_) => {}
                None => {
                    qb.push(" AND channel = ");
                    qb.push_bind(switch_channel.to_string());
                }
            }
        }
        scope::PRODUCER => {
            let Some(producer_id) = kill_switch.producer_id() else {
                return Ok(Vec::new());
            };
            qb.push(" AND producer_id = ");
            qb.push_bind(producer_id);
        }
        scope::PRODUCER_CHANNEL => {
            let Some((producer_id, switch_channel)) =
                kill_switch.producer_channel_parts()
            else {
                return Ok(Vec::new());
            };
            match channel {
                Some(pinned) if pinned != switch_channel => return Ok(Vec::new()),
                Some(_) => {}
                None => {
                    qb.push(" AND channel = ");
                    qb.push_bind(switch_channel.to_string());
                }
            }
            qb.push(" AND producer_id = ");
            qb.push_bind(producer_id);
        }
        scope::CAMPAIGN => {
            let Some(campaign_id) = kill_switch.scope_key.as_deref() else {
                return Ok(Vec::new());
            };
            qb.push(" AND campaign_id = ");
            qb.push_bind(campaign_id.to_string());
        }
        _ => return Ok(Vec::new()),
    }

    qb.push(" ORDER BY priority, next_attempt_at LIMIT ");
    qb.push_bind(limit);
    qb.push(
        " FOR UPDATE SKIP LOCKED) \
          RETURNING comms_request_id, created_at, channel, class, priority, customer_id, \
                    address_id, producer_id, campaign_id, next_attempt_at, expires_at, \
                    cancelled_at, attempts, leased_until",
    );

    qb.build_query_as::<ClaimedOutbox>().fetch_all(pool).await
}

/// Looks up the encrypted destination/payload for one ledger row.
/// `comms_request`'s primary key is `(created_at, id)` — `outbox.created_at`
/// is carried for exactly this join (its own column comment: "FK component
/// into ledger partition").
pub async fn load_ciphertexts(
    pool: &PgPool,
    created_at: DateTime<Utc>,
    comms_request_id: Uuid,
) -> Result<Option<RequestCiphertexts>, sqlx::Error> {
    sqlx::query_as::<_, RequestCiphertexts>(
        "SELECT destination_ciphertext, payload_ciphertext FROM comms_request \
         WHERE created_at = $1 AND id = $2",
    )
    .bind(created_at)
    .bind(comms_request_id)
    .fetch_optional(pool)
    .await
}

/// `customer_address.verified_at` for the verification gate (DESIGN.md §5,
/// T-036) — a missing row (no FK ties `outbox.address_id` to
/// `customer_address.id`, T-009 decision 1) collapses to the same
/// "unverified" answer as a row with `verified_at IS NULL`, via
/// `Option::flatten`.
pub async fn load_verified_at(
    pool: &PgPool,
    address_id: Uuid,
) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    let result: Option<Option<DateTime<Utc>>> =
        sqlx::query_scalar("SELECT verified_at FROM customer_address WHERE id = $1")
            .bind(address_id)
            .fetch_optional(pool)
            .await?;

    Ok(result.flatten())
}

/// `customer.timezone` for the quiet-hours gate (DESIGN.md §5, §6.1, T-043).
/// The column itself is `NOT NULL`, but there is no FK tying `outbox.customer_id`
/// to `customer.id` (T-009 decision 1, same absence `load_verified_at` notes for
/// `address_id`), so a missing row still collapses to `None` here -- the caller
/// falls back to the tenant's `default_timezone` either way (decision 2).
pub async fn load_customer_timezone(
    pool: &PgPool,
    customer_id: Uuid,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT timezone FROM customer WHERE id = $1")
        .bind(customer_id)
        .fetch_optional(pool)
        .await
}

/// A non-terminal `comms_event` row (T-036): unlike `write_terminal`, this
/// touches no `outbox`/`comms_request` state — used by the verification
/// gate's `observe` mode, which records the outcome but still lets the send
/// proceed. `provider_ref` is bound to `''`, matching `write_terminal`'s own
/// convention — the column is `NOT NULL DEFAULT ''` (migration 0004), so
/// `NULL` was never actually bindable here; `''` is simply the column's own
/// "no provider ref" value, not a dodge of the review addendum's
/// NULL-in-`UNIQUE`/`ON CONFLICT` warning (step 2), which that `NOT NULL`
/// constraint already forecloses.
pub async fn record_event(
    pool: &PgPool,
    comms_request_id: Uuid,
    customer_id: Uuid,
    event_type: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO comms_event (
            comms_request_id, customer_id, occurred_at, event_type, provider_ref,
            provider_status, provider_payload_ciphertext
        ) VALUES ($1, $2, $3, $4, '', NULL, NULL)
        ON CONFLICT (occurred_at, comms_request_id, event_type, provider_ref) DO NOTHING
        "#,
    )
    .bind(comms_request_id)
    .bind(customer_id)
    .bind(Utc::now())
    .bind(event_type)
    .execute(pool)
    .await?;

    Ok(())
}

/// Clears the lease and reschedules a retryable failure in one statement
/// (DESIGN.md's corrected §4.2, T-021 decision 3) — never a bare timeout
/// race between "write a terminal state" and "explicitly clear the lease on
/// a rescheduled retry". Writes no `comms_event` row: §2.4 step 6 names only
/// `attempts`/`next_attempt_at` for this path, "the row stays in the
/// outbox" — no event write, unlike the `sent`/`failed` terminal writes.
pub async fn reschedule_retry(
    pool: &PgPool,
    comms_request_id: Uuid,
    next_attempt_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE outbox SET leased_until = NULL, next_attempt_at = $2 \
         WHERE comms_request_id = $1",
    )
    .bind(comms_request_id)
    .bind(next_attempt_at)
    .execute(pool)
    .await?;

    Ok(())
}

/// Clears every stale lease for this tenant (T-021 decision 4) — called once,
/// immediately after a process wins tenant leadership (`leader::acquire`,
/// T-039), before any claim loop runs. Safe because winning the advisory lock
/// means Postgres has already ended the previous leader's session: any lease
/// still set at that moment belongs to a run that is no longer around to
/// finish it. `repo::claim`'s own predicate never compares `leased_until` to
/// `now()`, so without this sweep a row left leased by a crash (or any
/// pre-terminal-write failure) would stay leased forever rather than for the
/// nominal lease duration.
pub async fn clear_stale_leases(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE outbox SET leased_until = NULL WHERE leased_until IS NOT NULL",
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected())
}

/// Re-reads `outbox.cancelled_at` fresh from the DB (DESIGN.md §6.2, T-041)
/// — `ClaimedOutbox::cancelled_at` is a snapshot from claim time and
/// cannot see a cancel that landed afterward; this is the check that
/// closes the race between claim and send.
pub async fn is_cancelled(
    pool: &PgPool,
    comms_request_id: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT cancelled_at IS NOT NULL FROM outbox WHERE comms_request_id = $1",
    )
    .bind(comms_request_id)
    .fetch_one(pool)
    .await
}

/// Writes the one permitted `comms_request` mutation (§4.1), the
/// corresponding `comms_event` row (§4.4), and removes the row from the
/// queue (§4.2) — all in one transaction, so a crash between them can never
/// leave the ledger and the event stream disagreeing about whether this
/// attempt finished.
#[allow(clippy::too_many_arguments)]
pub async fn write_terminal(
    pool: &PgPool,
    created_at: DateTime<Utc>,
    comms_request_id: Uuid,
    customer_id: Uuid,
    event_type: &str,
    provider_ref: Option<&str>,
    provider_status: Option<&str>,
    final_status: &str,
) -> Result<(), sqlx::Error> {
    let now = Utc::now();
    let mut tx = pool.begin().await?;

    sqlx::query(
        r#"
        INSERT INTO comms_event (
            comms_request_id, customer_id, occurred_at, event_type, provider_ref,
            provider_status, provider_payload_ciphertext
        ) VALUES ($1, $2, $3, $4, $5, $6, NULL)
        ON CONFLICT (occurred_at, comms_request_id, event_type, provider_ref) DO NOTHING
        "#,
    )
    .bind(comms_request_id)
    .bind(customer_id)
    .bind(now)
    .bind(event_type)
    .bind(provider_ref.unwrap_or(""))
    .bind(provider_status)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "UPDATE comms_request SET final_status = $1, finalized_at = $2 \
         WHERE created_at = $3 AND id = $4",
    )
    .bind(final_status)
    .bind(now)
    .bind(created_at)
    .bind(comms_request_id)
    .execute(&mut *tx)
    .await?;

    sqlx::query("DELETE FROM outbox WHERE comms_request_id = $1")
        .bind(comms_request_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await
}

/// One round trip: joins the claimed row's own `comms_request.destination_hmac`
/// against `suppression`, so `try_process` never needs the raw HMAC bytes
/// itself (DESIGN.md §5, T-038). `review_at > now()` is the entire "still
/// blocking" condition -- an expired entry simply stops matching, no sweep
/// job needed (decision 2).
pub async fn is_suppressed(
    pool: &PgPool,
    created_at: DateTime<Utc>,
    comms_request_id: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM comms_request cr
            JOIN suppression s ON s.destination_hmac = cr.destination_hmac
            WHERE cr.created_at = $1 AND cr.id = $2 AND s.review_at > now()
        )
        "#,
    )
    .bind(created_at)
    .bind(comms_request_id)
    .fetch_one(pool)
    .await
}

/// The dispatcher's own consent check (DESIGN.md §5, T-037): absence of a
/// row means "not opted in," identical to an explicit `opted_in = false`
/// row (decision 4) -- callers never need to distinguish the two.
pub async fn is_consented(
    pool: &PgPool,
    address_id: Uuid,
    class: &str,
) -> Result<bool, sqlx::Error> {
    let opted_in: Option<bool> = sqlx::query_scalar(
        "SELECT opted_in FROM consent WHERE address_id = $1 AND class = $2",
    )
    .bind(address_id)
    .bind(class)
    .fetch_optional(pool)
    .await?;
    Ok(opted_in.unwrap_or(false))
}

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
    .bind(provider_ref)
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

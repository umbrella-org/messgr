//! Dispatcher leader election (T-039, closing build-order step 7's second
//! half; DESIGN.md §9, AGENTS.md hard invariant 9).
//!
//! `pg_try_advisory_lock` is a session-scoped lock: it is held for as long as
//! the connection that took it stays open, and Postgres releases it the
//! instant that connection's session ends (crash, explicit close, or an
//! ordinary drop). That is the entire mechanism here — `acquire` opens a
//! dedicated connection, tries the lock, and on success hands back an opaque
//! `Leadership` whose only job is to keep that connection alive. Dropping it
//! releases the lock and, with it, leadership.

use std::time::Duration;

use sqlx::Connection;
use sqlx::postgres::{PgConnectOptions, PgConnection};

/// Single fixed key — the lock namespace is already partitioned per tenant
/// database (§9), and there is one dispatcher process per tenant covering
/// every channel, so no per-tenant or per-channel derivation is needed.
pub const LEADER_LOCK_KEY: i64 = 1;

/// "The loser idles and retries every few seconds" (§9) — single-digit
/// seconds keeps failover inside the isolation suite's acceptance bar.
pub const RETRY_INTERVAL: Duration = Duration::from_secs(3);

/// Proof of leadership: holding this value is holding the advisory lock.
/// The lock is released the moment this is dropped, since dropping it closes
/// the connection it wraps and ends the session Postgres scoped the lock to.
pub struct Leadership {
    _conn: PgConnection,
}

/// Blocks until this process wins `LEADER_LOCK_KEY` on a fresh, dedicated
/// connection opened from `options`. Never returns `Err`: a standby that
/// loses the race (or hits a connection error) logs and retries every
/// `RETRY_INTERVAL` forever, matching "the loser idles and retries" (§9). A
/// connection string that can never succeed at all is already caught by
/// every other startup `.expect()` in `messgr-dispatcher` (Vault login,
/// tenant lookup, etc.), which all run regardless of leadership.
pub async fn acquire(options: PgConnectOptions) -> Leadership {
    loop {
        let mut conn = match PgConnection::connect_with(&options).await {
            Ok(conn) => conn,
            Err(err) => {
                tracing::warn!(%err, "messgr-dispatcher: leader-lock connection failed, retrying");
                tokio::time::sleep(RETRY_INTERVAL).await;
                continue;
            }
        };

        let won: Result<bool, sqlx::Error> =
            sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
                .bind(LEADER_LOCK_KEY)
                .fetch_one(&mut conn)
                .await;

        match won {
            Ok(true) => return Leadership { _conn: conn },
            Ok(false) => {
                tracing::info!(
                    "messgr-dispatcher: standing by, another instance holds leadership"
                );
            }
            Err(err) => {
                tracing::warn!(%err, "messgr-dispatcher: leader-lock query failed, retrying");
            }
        }

        tokio::time::sleep(RETRY_INTERVAL).await;
    }
}

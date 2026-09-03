//! Pool connection helpers.
//!
//! `connect` is used for the control pool, which has no "expected tenant" to
//! check against. `connect_with_expected_database` is used for every tenant
//! pool and carries DESIGN.md §2.1's `current_database()` assertion — the
//! only isolation mechanism this design uses, since there is no RLS and no
//! shared schema to filter. A pool is bound to a connection string at
//! creation, so the realistic failure mode is code reaching for the wrong
//! pool entirely, not a pool somehow changing databases underneath it; this
//! assertion catches exactly that.

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Executor, PgPool, Row};

pub async fn connect(
    database_url: &str,
    max_connections: u32,
) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(database_url)
        .await
}

/// Connects a pool and asserts, once, on the first physical connection, that
/// `current_database()` equals `expected_db`. A mismatch panics rather than
/// returning a swallowable error: a mis-wired tenant pool is exactly the bug
/// this check exists to make impossible to ignore. Fires only at creation —
/// a live connection can never change which database it's bound to mid-life,
/// so a checkout-time recheck could not observe anything this doesn't
/// already catch (DESIGN.md §2.1).
pub async fn connect_with_expected_database(
    options: PgConnectOptions,
    max_connections: u32,
    expected_db: &str,
) -> Result<PgPool, sqlx::Error> {
    let expected_for_connect = expected_db.to_string();

    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .after_connect(move |conn, _meta| {
            let expected = expected_for_connect.clone();
            Box::pin(async move {
                assert_current_database(conn, &expected).await?;
                Ok(())
            })
        })
        .connect_with(options)
        .await?;

    Ok(pool)
}

async fn assert_current_database(
    conn: &mut sqlx::PgConnection,
    expected: &str,
) -> Result<(), sqlx::Error> {
    let row = conn.fetch_one("SELECT current_database()").await?;
    let actual: String = row.get(0);

    assert_eq!(
        actual, expected,
        "tenant pool mis-routed: connected to database {actual:?}, expected {expected:?}"
    );

    Ok(())
}

/// Swaps in a tenant's database name on a base connection URL, preserving
/// every other connection option (TLS mode, application name, etc.) —
/// review finding T-001/F5. Parsing into `PgConnectOptions` and back out
/// through its own builder means nothing carried in the URL, including a
/// query string, is lost the way a naive string split would lose it.
pub fn with_database_name(
    base_url: &str,
    database_name: &str,
) -> Result<PgConnectOptions, sqlx::Error> {
    let options: PgConnectOptions = base_url.parse()?;
    Ok(options.database(database_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_database_name_swaps_the_database_and_keeps_everything_else() {
        let options = with_database_name(
            "postgres://messgr:messgr@localhost:5432/control",
            "tenant_acme",
        )
        .expect("parsing a well-formed URL must not fail");

        assert_eq!(options.get_database(), Some("tenant_acme"));
        assert_eq!(options.get_host(), "localhost");
        assert_eq!(options.get_port(), 5432);
        assert_eq!(options.get_username(), "messgr");
    }

    /// Review finding T-001/F5: the previous string-split implementation
    /// silently dropped every connection option carried in the query
    /// string. `sslmode=require` is the one that would have gone unnoticed
    /// in production — proven here by parsing it back out after the swap.
    #[test]
    fn with_database_name_preserves_query_string_options() {
        let options = with_database_name(
            "postgres://messgr:messgr@localhost:5432/control?sslmode=require",
            "tenant_acme",
        )
        .expect("parsing a well-formed URL must not fail");

        assert_eq!(options.get_database(), Some("tenant_acme"));
        assert_eq!(
            format!("{:?}", options.get_ssl_mode()),
            format!("{:?}", sqlx::postgres::PgSslMode::Require)
        );
    }

    /// `assert_current_database` is the private helper `after_connect` calls
    /// on every pool's first connection (DESIGN.md §2.1) — this proves it
    /// panics on a mismatch, using a connection acquired the normal way from
    /// an honestly-connected pool.
    ///
    /// Review finding T-001/F13: an earlier version of this test connected
    /// and acquired *inside* the spawned task, then asserted only
    /// `result.is_err()` — so an unrelated infrastructure failure (no
    /// database reachable at all) satisfied the assertion just as well as
    /// the assertion under test firing, and the test passed for the wrong
    /// reason with no database present. Fixed by connecting and acquiring
    /// *outside* the spawn (an infrastructure failure now fails the test via
    /// `expect`, the same way the honest integration tests in
    /// `tests/tenancy.rs` already do) and by asserting on the panic's actual
    /// message rather than merely its existence.
    #[tokio::test]
    async fn assert_current_database_panics_on_the_mismatch() {
        dotenvy::dotenv().ok();
        let url = std::env::var("CONTROL_DATABASE_URL")
            .expect("CONTROL_DATABASE_URL must be set for tests");

        let pool = super::connect(&url, 2)
            .await
            .expect("connecting the control pool failed");
        let mut conn = pool.acquire().await.expect("acquiring a connection failed");

        let result = tokio::spawn(async move {
            assert_current_database(&mut conn, "not_the_real_database").await
        })
        .await;

        let join_error = result.expect_err(
            "assert_current_database must panic on a mismatched expectation",
        );
        let panic_payload = join_error.into_panic();
        let message = panic_payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic_payload.downcast_ref::<&str>().map(|s| s.to_string()))
            .expect("panic payload was not a string message");
        assert!(
            message.contains("tenant pool mis-routed"),
            "panicked, but not with the current_database mismatch assertion's message: {message:?}"
        );
    }
}

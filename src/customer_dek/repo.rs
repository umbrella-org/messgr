use chrono::{DateTime, Utc};
use sqlx::PgPool;
use sqlx::postgres::PgTransaction;
use uuid::Uuid;

use super::model::CustomerDek;

pub async fn find(
    pool: &PgPool,
    customer_id: Uuid,
) -> Result<Option<CustomerDek>, sqlx::Error> {
    sqlx::query_as::<_, CustomerDek>(
        "SELECT customer_id, wrapped_dek, created_at, shredded_at FROM customer_dek WHERE customer_id = $1",
    )
    .bind(customer_id)
    .fetch_optional(pool)
    .await
}

/// Inserts a fresh DEK row unless one already exists for this customer.
/// Returns `true` if this call's row won (was actually inserted), `false` if
/// a concurrent caller (the lazy path racing the batch path) already had —
/// the caller is expected to `find` and use the winner's row instead of the
/// `Dek` it just discarded.
pub async fn insert_if_absent(
    pool: &PgPool,
    customer_id: Uuid,
    wrapped_dek: &str,
    created_at: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO customer_dek (customer_id, wrapped_dek, created_at) VALUES ($1, $2, $3) ON CONFLICT (customer_id) DO NOTHING",
    )
    .bind(customer_id)
    .bind(wrapped_dek)
    .bind(created_at)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() == 1)
}

/// Same as `insert_if_absent`, but inside a caller-owned transaction — so
/// the DEK row commits or rolls back atomically with whatever else that
/// transaction does (T-018: `customer_dek` must not survive a rolled-back
/// `customer`/`customer_address` insert, and must not commit any later than
/// they do either).
pub async fn insert_if_absent_tx(
    tx: &mut PgTransaction<'_>,
    customer_id: Uuid,
    wrapped_dek: &str,
    created_at: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO customer_dek (customer_id, wrapped_dek, created_at) VALUES ($1, $2, $3) ON CONFLICT (customer_id) DO NOTHING",
    )
    .bind(customer_id)
    .bind(wrapped_dek)
    .bind(created_at)
    .execute(&mut **tx)
    .await?;

    Ok(result.rows_affected() == 1)
}

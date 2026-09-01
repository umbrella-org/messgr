use chrono::{DateTime, Utc};
use sqlx::PgPool;
use sqlx::postgres::PgTransaction;
use uuid::Uuid;

use super::model::{Customer, CustomerAddress};

/// A `customer_alias` chain is a data bug, not something to hang on — this
/// bounds the number of hops `expand_alias` will follow before giving up.
const ALIAS_HOP_LIMIT: u8 = 8;

pub async fn find_by_id(
    pool: &PgPool,
    id: Uuid,
) -> Result<Option<Customer>, sqlx::Error> {
    sqlx::query_as::<_, Customer>(
        "SELECT id, locale, timezone, provisional, source_system, source_updated_at, created_at \
         FROM customer WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// Follows `customer_alias.old_customer_id -> customer_id` until no row
/// matches, up to `ALIAS_HOP_LIMIT` hops. Returns the input id unchanged the
/// moment no row matches (including immediately, for an id that was never
/// merged away).
pub async fn expand_alias(pool: &PgPool, id: Uuid) -> Result<Uuid, sqlx::Error> {
    let mut current = id;
    for _ in 0..ALIAS_HOP_LIMIT {
        let next: Option<Uuid> = sqlx::query_scalar(
            "SELECT customer_id FROM customer_alias WHERE old_customer_id = $1",
        )
        .bind(current)
        .fetch_optional(pool)
        .await?;

        match next {
            Some(next_id) => current = next_id,
            None => return Ok(current),
        }
    }
    Ok(current)
}

pub async fn find_customer_by_external_id(
    pool: &PgPool,
    system: &str,
    external_id: &str,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT customer_id FROM customer_external_id WHERE system = $1 AND external_id = $2",
    )
    .bind(system)
    .bind(external_id)
    .fetch_optional(pool)
    .await
}

/// Address-only resolution's lookup — global, no `customer_id` filter.
pub async fn find_active_address_by_hmac(
    pool: &PgPool,
    kind: &str,
    value_hmac: &[u8],
) -> Result<Option<CustomerAddress>, sqlx::Error> {
    sqlx::query_as::<_, CustomerAddress>(
        r#"
        SELECT id, customer_id, kind, value_ciphertext, value_hmac, rank, label,
               verified_at, active_from, active_to, source_updated_at
        FROM customer_address
        WHERE kind = $1 AND value_hmac = $2 AND active_to IS NULL
        "#,
    )
    .bind(kind)
    .bind(value_hmac)
    .fetch_optional(pool)
    .await
}

pub async fn find_active_address_for_customer(
    pool: &PgPool,
    customer_id: Uuid,
    kind: &str,
    value_hmac: &[u8],
) -> Result<Option<CustomerAddress>, sqlx::Error> {
    sqlx::query_as::<_, CustomerAddress>(
        r#"
        SELECT id, customer_id, kind, value_ciphertext, value_hmac, rank, label,
               verified_at, active_from, active_to, source_updated_at
        FROM customer_address
        WHERE customer_id = $1 AND kind = $2 AND value_hmac = $3 AND active_to IS NULL
        "#,
    )
    .bind(customer_id)
    .bind(kind)
    .bind(value_hmac)
    .fetch_optional(pool)
    .await
}

/// Callable only with an open transaction: the `FOR UPDATE` row lock it
/// takes is what makes two concurrent inserts for two different new
/// destinations on the same customer never collide on the same rank
/// (decision 8, third bullet).
///
/// The lock is taken on `customer_id`'s own row, not on its (possibly
/// empty) set of active address rows: `FOR UPDATE` cannot lock a row that
/// doesn't exist yet, so a customer's *first* address of a given `kind`
/// would otherwise be unprotected — two concurrent resolutions converging
/// on the same customer (e.g. two racing external-id lookups that both
/// land on the same winner) would both see zero existing rows, both
/// compute rank 1, and collide on `customer_address`'s
/// `(customer_id, kind, rank)` unique index. The customer row always
/// exists by this point (`customer_id` is already resolved), so locking it
/// serializes every concurrent address insert under this customer,
/// regardless of `kind` or whether any address rows exist yet.
pub async fn next_rank_for_update(
    tx: &mut PgTransaction<'_>,
    customer_id: Uuid,
    kind: &str,
) -> Result<i16, sqlx::Error> {
    sqlx::query("SELECT id FROM customer WHERE id = $1 FOR UPDATE")
        .bind(customer_id)
        .fetch_one(&mut **tx)
        .await?;

    let max_rank: Option<i16> = sqlx::query_scalar(
        "SELECT MAX(rank) FROM customer_address \
         WHERE customer_id = $1 AND kind = $2 AND active_to IS NULL",
    )
    .bind(customer_id)
    .bind(kind)
    .fetch_one(&mut **tx)
    .await?;

    Ok(max_rank.unwrap_or(0) + 1)
}

/// Returns whether this call's row won the insert. `false` means a
/// concurrent caller already claimed `id` — the only caller that can
/// observe this is the explicit-`customer_id` mint path (decision 6),
/// since it's the only one that inserts under a caller-supplied id rather
/// than a freshly generated one; a fresh `Uuid::new_v4()` colliding here is
/// not a real possibility, so the other mint paths discard the result.
#[allow(clippy::too_many_arguments)]
pub async fn insert_customer(
    tx: &mut PgTransaction<'_>,
    id: Uuid,
    locale: &str,
    timezone: &str,
    provisional: bool,
    source_updated_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        r#"
        INSERT INTO customer (id, locale, timezone, provisional, source_system, source_updated_at, created_at)
        VALUES ($1, $2, $3, $4, NULL, $5, $6)
        ON CONFLICT (id) DO NOTHING
        "#,
    )
    .bind(id)
    .bind(locale)
    .bind(timezone)
    .bind(provisional)
    .bind(source_updated_at)
    .bind(created_at)
    .execute(&mut **tx)
    .await?;

    Ok(result.rows_affected() == 1)
}

/// Returns whether the row was actually inserted — `false` means a
/// concurrent caller already claimed this `(system, external_id)` pair
/// (decision 8, second bullet).
pub async fn insert_external_id(
    tx: &mut PgTransaction<'_>,
    system: &str,
    external_id: &str,
    customer_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO customer_external_id (customer_id, system, external_id) VALUES ($1, $2, $3) \
         ON CONFLICT (system, external_id) DO NOTHING",
    )
    .bind(customer_id)
    .bind(system)
    .bind(external_id)
    .execute(&mut **tx)
    .await?;

    Ok(result.rows_affected() == 1)
}

/// Returns whether the row was actually inserted — `false` means the
/// `(kind, value_hmac)` unique index rejected it; the caller re-fetches and
/// compares `customer_id` to distinguish "lost the race" (decision 8) from
/// "conflicts with a different customer" (decision 9).
#[allow(clippy::too_many_arguments)]
pub async fn insert_address(
    tx: &mut PgTransaction<'_>,
    id: Uuid,
    customer_id: Uuid,
    kind: &str,
    value_ciphertext: &[u8],
    value_hmac: &[u8],
    rank: i16,
    active_from: DateTime<Utc>,
    source_updated_at: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        r#"
        INSERT INTO customer_address (
            id, customer_id, kind, value_ciphertext, value_hmac, rank, label,
            verified_at, active_from, active_to, source_updated_at
        ) VALUES ($1, $2, $3, $4, $5, $6, NULL, NULL, $7, NULL, $8)
        ON CONFLICT (kind, value_hmac) WHERE active_to IS NULL DO NOTHING
        "#,
    )
    .bind(id)
    .bind(customer_id)
    .bind(kind)
    .bind(value_ciphertext)
    .bind(value_hmac)
    .bind(rank)
    .bind(active_from)
    .bind(source_updated_at)
    .execute(&mut **tx)
    .await?;

    Ok(result.rows_affected() == 1)
}

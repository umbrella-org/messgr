use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::model::{Consent, ConsentInput};

pub async fn load_one(
    pool: &PgPool,
    address_id: Uuid,
    class: &str,
) -> Result<Option<Consent>, sqlx::Error> {
    sqlx::query_as::<_, Consent>(
        "SELECT address_id, class, opted_in, source, updated_at FROM consent \
         WHERE address_id = $1 AND class = $2",
    )
    .bind(address_id)
    .bind(class)
    .fetch_optional(pool)
    .await
}

pub async fn upsert(
    pool: &PgPool,
    input: &ConsentInput,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO consent (address_id, class, opted_in, source, updated_at)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (address_id, class) DO UPDATE SET
            opted_in = EXCLUDED.opted_in,
            source = EXCLUDED.source,
            updated_at = EXCLUDED.updated_at
        "#,
    )
    .bind(input.address_id)
    .bind(&input.class)
    .bind(input.opted_in)
    .bind(&input.source)
    .bind(now)
    .execute(pool)
    .await
    .map(|_| ())
}

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::model::AccessAuditInput;

pub async fn record(
    pool: &PgPool,
    input: &AccessAuditInput,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO access_audit (id, occurred_at, actor, role, route, customer_id, query_params)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(now)
    .bind(&input.actor)
    .bind(&input.role)
    .bind(&input.route)
    .bind(input.customer_id)
    .bind(&input.query_params)
    .execute(pool)
    .await
    .map(|_| ())
}

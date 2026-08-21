use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::model::SmsMessage;

pub async fn batch_insert(
    pool: &PgPool,
    batch: &[SmsMessage],
) -> Result<(), sqlx::Error> {
    let ids: Vec<Uuid> = batch.iter().map(|m| m.id).collect();
    let senders: Vec<&str> = batch.iter().map(|m| m.sender.as_str()).collect();
    let recipients: Vec<&str> = batch.iter().map(|m| m.recipient.as_str()).collect();
    let bodies: Vec<&str> = batch.iter().map(|m| m.body.as_str()).collect();
    let received_ats: Vec<DateTime<Utc>> =
        batch.iter().map(|m| m.received_at).collect();

    sqlx::query(
        r#"
        INSERT INTO sms_messages (id, sender, recipient, body, received_at)
        SELECT * FROM UNNEST($1::uuid[], $2::varchar[], $3::varchar[], $4::text[], $5::timestamptz[])
        "#,
    )
    .bind(ids)
    .bind(senders)
    .bind(recipients)
    .bind(bodies)
    .bind(received_ats)
    .execute(pool)
    .await
    .map(|_| ())
}

const LIST_LIMIT: i64 = 100;

pub async fn list(pool: &PgPool) -> Result<Vec<SmsMessage>, sqlx::Error> {
    sqlx::query_as::<_, SmsMessage>(
        "SELECT id, sender, recipient, body, received_at FROM sms_messages ORDER BY received_at DESC LIMIT $1",
    )
    .bind(LIST_LIMIT)
    .fetch_all(pool)
    .await
}

pub async fn search(
    pool: &PgPool,
    query: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<SmsMessage>, sqlx::Error> {
    let pattern = format!("%{query}%");

    sqlx::query_as::<_, SmsMessage>(
        r#"
        SELECT id, sender, recipient, body, received_at
        FROM sms_messages
        WHERE sender ILIKE $1 OR recipient ILIKE $1 OR body ILIKE $1
        ORDER BY received_at DESC
        LIMIT $2 OFFSET $3
        "#,
    )
    .bind(pattern)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
}

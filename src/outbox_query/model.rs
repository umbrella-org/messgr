use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

/// One pending `outbox` row (T-049 decision 6) — the small live queue, not
/// `comms_request`'s 7-year partitioned ledger. Cancelling a row calls
/// `ingest::repo::cancel(pool, comms_request_id, producer_id)`, which the
/// listing already carries `producer_id` for.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct OutboxQueueRow {
    pub comms_request_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub channel: String,
    pub class: String,
    pub producer_id: Uuid,
    pub campaign_id: Option<String>,
    pub next_attempt_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

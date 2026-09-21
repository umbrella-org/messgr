use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

/// The admin panel's scheduled-queue view's query parameters (T-049 decision
/// 6) — every field optional, applied the same `($n::type IS NULL OR
/// column = $n)` way `comms_query::filter::CommsFilter` already does.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OutboxQueueFilter {
    pub producer_id: Option<Uuid>,
    pub campaign_id: Option<String>,
    pub due_before: Option<DateTime<Utc>>,
    pub due_after: Option<DateTime<Utc>>,
}

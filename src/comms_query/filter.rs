use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

/// `GET /comms`'s query parameters, used via axum's `Query` extractor. Every
/// field optional — `comms_query::repo::list` applies each as
/// `($n::type IS NULL OR column = $n)`, no dynamic query builder (T-048
/// Task 5).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CommsFilter {
    pub customer_id: Option<Uuid>,
    pub channel: Option<String>,
    pub class: Option<String>,
    pub campaign_id: Option<String>,
    pub producer_id: Option<Uuid>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    /// Filters on `comms_request.final_status`.
    pub status: Option<String>,
    /// `Some(true)` maps to `scheduled_for IS NOT NULL AND final_status IS
    /// NULL` (still-pending scheduled sends); `Some(false)` its negation;
    /// `None` applies no filter.
    pub scheduled: Option<bool>,
}

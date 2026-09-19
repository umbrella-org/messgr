use chrono::{DateTime, Utc};
use uuid::Uuid;

pub mod enforcement {
    pub const HARD: &str = "hard";
    pub const SOFT: &str = "soft";
}

pub mod granularity {
    pub const MINUTE: &str = "minute";
    pub const DAY: &str = "day";
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProducerQuota {
    pub producer_id: Uuid,
    pub channel: String,
    pub class: String,
    pub per_minute: Option<i32>,
    pub per_day: Option<i32>,
    pub enforcement: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProducerQuotaInput {
    pub producer_id: Uuid,
    pub channel: String,
    pub class: String,
    pub per_minute: Option<i32>,
    pub per_day: Option<i32>,
    pub enforcement: String,
}

impl ProducerQuotaInput {
    pub fn matches(&self, existing: &ProducerQuota) -> bool {
        self.producer_id == existing.producer_id
            && self.channel == existing.channel
            && self.class == existing.class
            && self.per_minute == existing.per_minute
            && self.per_day == existing.per_day
            && self.enforcement == existing.enforcement
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProducerQuotaOverride {
    pub id: Uuid,
    pub producer_id: Uuid,
    pub channel: String,
    pub class: String,
    pub per_day: i32,
    pub valid_from: DateTime<Utc>,
    pub valid_to: DateTime<Utc>,
    pub approved_by: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct ProducerQuotaOverrideInput {
    pub producer_id: Uuid,
    pub channel: String,
    pub class: String,
    pub per_day: i32,
    pub valid_from: DateTime<Utc>,
    pub valid_to: DateTime<Utc>,
    pub approved_by: String,
    pub reason: String,
}

/// One `producer_usage` row, either a live in-process snapshot being
/// flushed or a row read back at startup to rebuild one.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UsageRow {
    pub producer_id: Uuid,
    pub channel: String,
    pub class: String,
    pub granularity: String,
    pub window_start: DateTime<Utc>,
    pub sent: i64,
    pub blocked: i64,
}

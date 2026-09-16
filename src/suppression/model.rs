use chrono::{DateTime, Utc};

pub mod reason {
    pub const HARD_BOUNCE: &str = "hard_bounce";
    pub const COMPLAINT: &str = "complaint";
    pub const REGULATORY_HOLD: &str = "regulatory_hold";
}

/// Mirrors the `suppression` table (DESIGN.md §5, §4.4, T-038). Keyed on
/// `destination_hmac` -- deliberately the raw address hash, not `address_id`
/// or `customer_id` -- so a recycled destination cannot inherit or escape a
/// suppression entry that belongs to whoever held it before.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Suppression {
    pub destination_hmac: Vec<u8>,
    pub reason: String,
    pub added_at: DateTime<Utc>,
    pub review_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct SuppressionInput {
    pub destination_hmac: Vec<u8>,
    pub reason: String,
    pub review_at: DateTime<Utc>,
}

impl SuppressionInput {
    /// `added_at` is excluded deliberately -- it's server-set on first
    /// insert and never part of caller input, so a re-`add` can only ever
    /// change `reason`/`review_at`.
    pub fn matches(&self, existing: &Suppression) -> bool {
        self.destination_hmac == existing.destination_hmac
            && self.reason == existing.reason
            && self.review_at == existing.review_at
    }
}

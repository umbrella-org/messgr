use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Mirrors the `consent` table (DESIGN.md §5, §4.4, T-037). Keyed on
/// `(address_id, class)` (AGENTS.md hard invariant 4) -- never `customer_id`
/// or the raw destination -- so a recycled address's new `customer_address`
/// row starts with no consent record of its own.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Consent {
    pub address_id: Uuid,
    pub class: String,
    pub opted_in: bool,
    pub source: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct ConsentInput {
    pub address_id: Uuid,
    pub class: String,
    pub opted_in: bool,
    pub source: String,
}

impl ConsentInput {
    /// `updated_at` is excluded deliberately -- it's server-set on every
    /// write, so a re-`set` can only ever change `opted_in`/`source`.
    pub fn matches(&self, existing: &Consent) -> bool {
        self.address_id == existing.address_id
            && self.class == existing.class
            && self.opted_in == existing.opted_in
            && self.source == existing.source
    }
}

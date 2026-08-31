use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Mirrors the `customer_dek` table (DESIGN.md §4.5) in the tenant database.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CustomerDek {
    pub customer_id: Uuid,
    pub wrapped_dek: String,
    pub created_at: DateTime<Utc>,
    pub shredded_at: Option<DateTime<Utc>>,
}

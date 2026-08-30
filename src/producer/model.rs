use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Mirrors the `producer` table (DESIGN.md §4.9) in the tenant database. No
/// `tenant_id` column — the tenant is the database (§2.1).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Producer {
    pub id: Uuid,
    pub name: String,
    pub cert_subject: String,
    pub owner_team: String,
    pub contact: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

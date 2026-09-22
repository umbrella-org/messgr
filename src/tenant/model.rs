use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Mirrors the `tenant` table (DESIGN.md §4.11) in the control database.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Tenant {
    pub id: Uuid,
    pub slug: String,
    pub region: String,
    pub database_name: String,
    pub vault_mount: String,
    pub vault_role_id: Option<String>,
    pub vault_pepper_wrapped: Option<String>,
    pub webhook_token: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
}

pub mod status {
    pub const PROVISIONING: &str = "provisioning";
    pub const ACTIVE: &str = "active";
    #[allow(dead_code)] // referenced by §7.7 offboarding, not built yet
    pub const SUSPENDED: &str = "suspended";
    #[allow(dead_code)]
    pub const OFFBOARDING_ARCHIVE: &str = "offboarding_archive";
    pub const OFFBOARDING_DESTROY: &str = "offboarding_destroy";
}

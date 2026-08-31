use chrono::{DateTime, Utc};

/// Mirrors the `template` table (DESIGN.md §4.4) in the tenant database. No
/// `tenant_id` column — the tenant is the database (§2.1). No `status`/draft
/// field — every row is already approved (T-010 decision 1).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Template {
    pub template_id: String,
    pub version: i32,
    pub channel: String,
    pub locale: String,
    pub body: String,
    pub approved_by: String,
    pub approved_at: DateTime<Utc>,
}

/// `channel`'s closed set of legal values (DESIGN.md §4.4's own comment:
/// `sms | email | whatsapp`). A plain `String` column + `&'static str`
/// constants, matching `verification_mode`'s convention rather than
/// introducing a new Rust enum for a DB-backed value (T-010 decision 4).
pub mod channel {
    pub const SMS: &str = "sms";
    pub const EMAIL: &str = "email";
    pub const WHATSAPP: &str = "whatsapp";
}

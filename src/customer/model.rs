use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Mirrors the `customer` table (DESIGN.md §4.6). Never the system of
/// record — a read-only projection, held only to resolve who to send to.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Customer {
    pub id: Uuid,
    pub locale: String,
    pub timezone: String,
    pub provisional: bool,
    pub source_system: Option<String>,
    pub source_updated_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Mirrors the `customer_address` table (DESIGN.md §4.6). Append-only:
/// an update closes `active_to` and inserts a successor row rather than
/// mutating this one.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CustomerAddress {
    pub id: Uuid,
    pub customer_id: Uuid,
    pub kind: String,
    pub value_ciphertext: Vec<u8>,
    pub value_hmac: Vec<u8>,
    pub rank: i16,
    pub label: Option<String>,
    pub verified_at: Option<DateTime<Utc>>,
    pub active_from: DateTime<Utc>,
    pub active_to: Option<DateTime<Utc>>,
    pub source_updated_at: DateTime<Utc>,
}

pub mod kind {
    pub const EMAIL: &str = "email";
    pub const MSISDN: &str = "msisdn";
    pub const WHATSAPP: &str = "whatsapp";
    #[allow(dead_code)] // no push/postal channel exists yet (step 11)
    pub const PUSH: &str = "push";
    #[allow(dead_code)]
    pub const POSTAL: &str = "postal";
}

pub fn kind_for_channel(channel: &str) -> &'static str {
    match channel {
        crate::ingest::model::channel::SMS => kind::MSISDN,
        crate::ingest::model::channel::EMAIL => kind::EMAIL,
        crate::ingest::model::channel::WHATSAPP => kind::WHATSAPP,
        other => unreachable!("validate_channel already rejected {other:?}"),
    }
}

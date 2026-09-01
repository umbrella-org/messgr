//! Row shapes for the claim loop (DESIGN.md §4.2, §4.1, T-013).

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// A leased `outbox` row, as returned by `repo::claim`'s `RETURNING` clause
/// — mirrors the table (`migrations/tenant/0004_ledger_outbox_schema.sql`)
/// exactly.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ClaimedOutbox {
    pub comms_request_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub channel: String,
    pub class: String,
    pub priority: i16,
    pub customer_id: Uuid,
    pub address_id: Uuid,
    pub producer_id: Uuid,
    pub campaign_id: Option<String>,
    pub next_attempt_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub cancelled_at: Option<DateTime<Utc>>,
    pub attempts: i16,
    pub leased_until: Option<DateTime<Utc>>,
}

/// The two encrypted columns on `comms_request` a send needs decrypted
/// (§7.1) — everything else the dispatcher needs already travelled on the
/// `outbox` row itself.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RequestCiphertexts {
    pub destination_ciphertext: Vec<u8>,
    pub payload_ciphertext: Option<Vec<u8>>,
}

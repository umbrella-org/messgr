use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

/// One `comms_request` row without its ciphertext columns — the shape every
/// list/timeline view returns (T-048 Task 5).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct CommsRequestSummary {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub customer_id: Uuid,
    pub channel: String,
    pub class: String,
    pub template_id: String,
    pub template_version: i32,
    pub campaign_id: Option<String>,
    pub producer_id: Uuid,
    pub scheduled_for: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub final_status: Option<String>,
    pub finalized_at: Option<DateTime<Utc>>,
}

/// A single `comms_request` row with its ciphertext columns still wrapped —
/// decrypting them is the caller's job (T-048 decision 14), not
/// `comms_query::repo`'s.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CommsRequestDetail {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub customer_id: Uuid,
    pub channel: String,
    pub class: String,
    pub template_id: String,
    pub template_version: i32,
    pub campaign_id: Option<String>,
    pub destination_ciphertext: Vec<u8>,
    pub payload_ciphertext: Option<Vec<u8>>,
    pub producer_id: Uuid,
    pub scheduled_for: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub final_status: Option<String>,
    pub finalized_at: Option<DateTime<Utc>>,
}

/// The decrypted form of `CommsRequestDetail`, what handlers/views actually
/// render.
#[derive(Debug, Clone, Serialize)]
pub struct CommsRequestDetailView {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub customer_id: Uuid,
    pub channel: String,
    pub class: String,
    pub template_id: String,
    pub template_version: i32,
    pub campaign_id: Option<String>,
    pub destination: String,
    /// `None` for `class = "auth"` rows, which never carry a payload.
    pub body: Option<String>,
    pub producer_id: Uuid,
    pub scheduled_for: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub final_status: Option<String>,
    pub finalized_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct CommsEventRow {
    pub comms_request_id: Uuid,
    pub customer_id: Uuid,
    pub occurred_at: DateTime<Utc>,
    pub event_type: String,
    pub provider_ref: String,
    pub provider_status: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CampaignReachCount {
    pub final_status: Option<String>,
    pub count: i64,
}

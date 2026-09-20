use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Mirrors the `access_audit` table (DESIGN.md §11.1, T-048). Named
/// exemption from erasure (`tests/erasure_coverage.rs`, decision 8): evidence
/// of what a compliance user did, not the customer's own data.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AccessAudit {
    pub id: Uuid,
    pub occurred_at: DateTime<Utc>,
    pub actor: String,
    pub role: String,
    pub route: String,
    pub customer_id: Option<Uuid>,
    pub query_params: String,
}

#[derive(Debug, Clone)]
pub struct AccessAuditInput {
    pub actor: String,
    pub role: String,
    pub route: String,
    pub customer_id: Option<Uuid>,
    pub query_params: String,
}

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct SmsMessage {
    pub id: Uuid,
    pub sender: String,
    pub recipient: String,
    pub body: String,
    pub received_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct CreateSmsMessage {
    pub sender: String,
    pub recipient: String,
    pub body: String,
}

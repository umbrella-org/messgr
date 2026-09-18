use chrono::NaiveTime;

pub const DEFAULT_SCOPE: &str = "default";
pub const DEFAULT_SCOPE_KEY: &str = "";

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct QuietHoursPolicy {
    pub scope: String,
    pub scope_key: String,
    pub start_local: NaiveTime,
    pub end_local: NaiveTime,
}

#[derive(Debug, Clone)]
pub struct QuietHoursPolicyInput {
    pub start_local: NaiveTime,
    pub end_local: NaiveTime,
}

impl QuietHoursPolicyInput {
    pub fn matches(&self, existing: &QuietHoursPolicy) -> bool {
        self.start_local == existing.start_local && self.end_local == existing.end_local
    }
}

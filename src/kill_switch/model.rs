//! `kill_switch` row shape (DESIGN.md §5.2, §4.9, T-016) — mirrors
//! `migrations/tenant/0009_kill_switch.sql` exactly.

use chrono::{DateTime, Utc};
use uuid::Uuid;

pub mod scope {
    pub const GLOBAL: &str = "global";
    pub const CHANNEL: &str = "channel";
    pub const PRODUCER: &str = "producer";
    pub const PRODUCER_CHANNEL: &str = "producer_channel";
    pub const CAMPAIGN: &str = "campaign";
}

pub mod on_queued {
    pub const HOLD: &str = "hold";
    pub const DISCARD: &str = "discard";
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct KillSwitch {
    pub id: Uuid,
    pub scope: String,
    pub scope_key: Option<String>,
    pub on_queued: String,
    pub engaged_by: String,
    pub engaged_at: DateTime<Utc>,
    pub reason: String,
    pub released_by: Option<String>,
    pub released_at: Option<DateTime<Utc>>,
}

/// A `producer_channel`-scope switch packs both a producer id and a channel
/// into the one `scope_key` text column — there is nowhere else to put a
/// second key. The literal format is `"<producer_id>:<channel>"`;
/// `docs/user-manual/kill-switches.adoc` documents the exact value a `psql`
/// operator must write.
fn split_producer_channel(scope_key: &str) -> Option<(Uuid, &str)> {
    let (producer, channel) = scope_key.split_once(':')?;
    let producer_id = Uuid::parse_str(producer).ok()?;
    Some((producer_id, channel))
}

impl KillSwitch {
    pub fn is_active(&self) -> bool {
        self.released_at.is_none()
    }

    /// Whether an outbox row on `channel`, from `producer_id`, in
    /// `campaign_id`, falls under this switch's scope — used by ingest's
    /// per-request check and by the dispatcher's claim-exclusion builder.
    pub fn matches(&self, channel: &str, producer_id: Uuid, campaign_id: Option<&str>) -> bool {
        match self.scope.as_str() {
            scope::GLOBAL => true,
            scope::CHANNEL => self.scope_key.as_deref() == Some(channel),
            scope::PRODUCER => self.producer_id().is_some_and(|id| id == producer_id),
            scope::PRODUCER_CHANNEL => self
                .producer_channel_parts()
                .is_some_and(|(id, ch)| id == producer_id && ch == channel),
            scope::CAMPAIGN => campaign_id.is_some() && self.scope_key.as_deref() == campaign_id,
            _ => false,
        }
    }

    /// The `producer_id` a `producer`-scope switch's `scope_key` encodes, or
    /// `None` if it doesn't parse (a malformed row from outside the runbook —
    /// treated as matching nothing rather than panicking).
    pub fn producer_id(&self) -> Option<Uuid> {
        self.scope_key.as_deref().and_then(|k| Uuid::parse_str(k).ok())
    }

    /// The `(producer_id, channel)` pair a `producer_channel`-scope switch's
    /// `scope_key` encodes.
    pub fn producer_channel_parts(&self) -> Option<(Uuid, &str)> {
        self.scope_key.as_deref().and_then(split_producer_channel)
    }
}

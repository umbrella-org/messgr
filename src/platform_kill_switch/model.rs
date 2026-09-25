//! `platform_kill_switch` row shape — mirrors
//! `migrations/control/0001_control_schema.sql` plus the constraints
//! `0006_platform_kill_switch_constraints.sql` adds (T-058).

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::kill_switch::model::{KillSwitch, on_queued, scope as tenant_scope};

pub mod scope {
    /// Region-wide: blocks every tenant; `tenant_id` is `NULL`.
    pub const PLATFORM: &str = "platform";
    /// One tenant; `tenant_id` is set.
    pub const TENANT: &str = "tenant";
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PlatformKillSwitch {
    pub id: Uuid,
    pub scope: String,
    pub tenant_id: Option<Uuid>,
    pub engaged_by: String,
    pub engaged_at: DateTime<Utc>,
    pub reason: String,
    pub released_by: Option<String>,
    pub released_at: Option<DateTime<Utc>>,
}

impl PlatformKillSwitch {
    pub fn is_active(&self) -> bool {
        self.released_at.is_none()
    }

    /// Whether this switch blocks `tenant_id`: every region-wide switch
    /// does, a tenant switch only its own tenant. An unrecognised `scope`
    /// (impossible past 0006's CHECK) matches nothing.
    pub fn applies_to(&self, tenant_id: Uuid) -> bool {
        match self.scope.as_str() {
            scope::PLATFORM => true,
            scope::TENANT => self.tenant_id == Some(tenant_id),
            _ => false,
        }
    }

    /// The tenant-tier row `KillSwitchCache` carries for this switch (T-058
    /// decision 1): `global` scope, `hold`, same id. A platform switch then
    /// excludes everything from the claim loop exactly as a tenant `global`
    /// switch does, and its release shows up in `RefreshDelta::released`,
    /// so the dispatcher's release-drain ramp (DESIGN.md §5.2, decision 12)
    /// applies to it without a second drain path. Always `hold`, never
    /// `discard`: a suspension must not touch the tenant's data.
    pub fn as_kill_switch(&self) -> KillSwitch {
        KillSwitch {
            id: self.id,
            scope: tenant_scope::GLOBAL.to_string(),
            scope_key: None,
            on_queued: on_queued::HOLD.to_string(),
            engaged_by: self.engaged_by.clone(),
            engaged_at: self.engaged_at,
            reason: self.reason.clone(),
            released_by: self.released_by.clone(),
            released_at: self.released_at,
        }
    }
}

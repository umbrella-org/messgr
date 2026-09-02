use sqlx::postgres::types::PgInterval;

/// Mirrors the `tenant_config` table (DESIGN.md §4.10) in the tenant
/// database. No `tenant_id` column and no `singleton` field — the tenant is
/// the database (§2.1), and the table holds at most one row by construction
/// (T-007 decision 3).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TenantConfig {
    pub retention_years: i32,
    pub default_timezone: String,
    pub default_locale: String,
    pub schedule_horizon_days: i32,
    pub quota_day_boundary_tz: String,
    pub verification_mode: String,
    pub staleness_max_age: PgInterval,
    /// Rows/second the kill-switch release-drain ramp admits after a switch
    /// releases (DESIGN.md §5.2, T-016 decision 5) — a fresh tenant with no
    /// `tenant_config` row has no value here at all (T-007 decision 4);
    /// `messgr-dispatcher` falls back to this column's own SQL default.
    pub kill_switch_release_rate: i32,
}

impl TenantConfig {
    /// Converts `staleness_max_age` to a `chrono::Duration`. Valid only
    /// because every writer of this column (`repo::upsert`) always leaves
    /// `months` and `days` at zero (T-007 decision 6) — a freshness bound
    /// has no calendar-length component to represent, so only
    /// `microseconds` is ever populated.
    pub fn staleness_max_age_duration(&self) -> chrono::Duration {
        chrono::Duration::microseconds(self.staleness_max_age.microseconds)
    }
}

/// The values `repo::upsert` writes. Kept distinct from `TenantConfig`
/// (rather than reusing it) so `configure::set_tenant_config` can compare a
/// freshly loaded `TenantConfig` against a caller-supplied `TenantConfigInput`
/// field by field without the `singleton` column ever entering either type.
#[derive(Debug, Clone, PartialEq)]
pub struct TenantConfigInput {
    pub retention_years: i32,
    pub default_timezone: String,
    pub default_locale: String,
    pub schedule_horizon_days: i32,
    pub quota_day_boundary_tz: String,
    pub verification_mode: String,
    pub staleness_max_age: PgInterval,
    pub kill_switch_release_rate: i32,
}

impl TenantConfigInput {
    /// Whether `existing` (a loaded `TenantConfig`) already holds exactly
    /// these values — used by `configure::set_tenant_config` to distinguish
    /// an `updated` write from an `idempotent` no-op.
    pub fn matches(&self, existing: &TenantConfig) -> bool {
        self.retention_years == existing.retention_years
            && self.default_timezone == existing.default_timezone
            && self.default_locale == existing.default_locale
            && self.schedule_horizon_days == existing.schedule_horizon_days
            && self.quota_day_boundary_tz == existing.quota_day_boundary_tz
            && self.verification_mode == existing.verification_mode
            && self.staleness_max_age == existing.staleness_max_age
            && self.kill_switch_release_rate == existing.kill_switch_release_rate
    }
}

/// `verification_mode`'s closed set of legal values (DESIGN.md §5). A plain
/// `String` column + `&'static str` constants, matching
/// `tenant::model::status`'s convention rather than introducing the
/// codebase's first Rust enum for a DB-backed value (T-007 decision 5).
pub mod verification_mode {
    pub const ENFORCE: &str = "enforce";
    pub const OBSERVE: &str = "observe";
}

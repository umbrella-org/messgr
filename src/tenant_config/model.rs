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
    /// Rows/second the kill-switch release-drain ramp admits after a switch
    /// releases (DESIGN.md §5.2, T-016 decision 5) — a fresh tenant with no
    /// `tenant_config` row has no value here at all (T-007 decision 4);
    /// `messgr-dispatcher` falls back to this column's own SQL default.
    pub kill_switch_release_rate: i32,
    /// How many reconcile passes a pending `orphan_event` row survives
    /// before `orphan_reconcile::reconcile::run` ages it out and deletes it
    /// (DESIGN.md §4.4/§10 correction note, T-033) — default 5, matching
    /// this column's own SQL default; a fresh tenant with no `tenant_config`
    /// row falls back to that same default via the consumer, not
    /// auto-seeding.
    pub reconcile_attempts_cap: i16,
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
    pub kill_switch_release_rate: i32,
    pub reconcile_attempts_cap: i16,
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
            && self.kill_switch_release_rate == existing.kill_switch_release_rate
            && self.reconcile_attempts_cap == existing.reconcile_attempts_cap
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

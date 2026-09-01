/// Mirrors the `provider_config` table (DESIGN.md §4.10) in the tenant
/// database. No `tenant_id` column — the tenant is the database (§2.1),
/// matching `producer`/`tenant_config` (T-012 decision 2, correcting
/// DESIGN.md's own snippet, which still shows one).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProviderConfig {
    pub channel: String,
    pub priority: i16,
    pub provider: String,
    pub credential_path: String,
    pub rate_limit_per_sec: i32,
}

/// The values `repo::upsert` writes. Kept distinct from `ProviderConfig`
/// (rather than reusing it) so `configure::set_provider_config` can compare
/// a freshly loaded `ProviderConfig` against a caller-supplied
/// `ProviderConfigInput` field by field.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderConfigInput {
    pub channel: String,
    pub priority: i16,
    pub provider: String,
    pub credential_path: String,
    pub rate_limit_per_sec: i32,
}

impl ProviderConfigInput {
    /// Whether `existing` (a loaded `ProviderConfig`) already holds exactly
    /// these values — used by `configure::set_provider_config` to
    /// distinguish an `updated` write from an `idempotent` no-op.
    pub fn matches(&self, existing: &ProviderConfig) -> bool {
        self.channel == existing.channel
            && self.priority == existing.priority
            && self.provider == existing.provider
            && self.credential_path == existing.credential_path
            && self.rate_limit_per_sec == existing.rate_limit_per_sec
    }
}

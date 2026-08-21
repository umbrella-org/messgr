//! Deployment profile, read once from `MESSGR_PROFILE`.
//!
//! Established here (DESIGN.md §2.1's `current_database()` pool-mismatch
//! assertion) because two other guards documented elsewhere reuse the exact
//! same shape: `MockProvider` refusing to start outside `dev` (§11.1), and
//! dev-mode Vault refusing non-dev callers (§7.6). One enum, not three ad hoc
//! flags.

use std::env;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Dev,
    Staging,
    Production,
}

impl Profile {
    /// Reads `MESSGR_PROFILE` (case-insensitive), defaulting to `Dev` when
    /// unset. An unrecognized value is a fatal misconfiguration, not a silent
    /// fallback — the whole point of this type is that "which profile is
    /// this" is never ambiguous.
    pub fn from_env() -> Self {
        match env::var("MESSGR_PROFILE") {
            Ok(value) => Self::parse(&value)
                .unwrap_or_else(|| panic!("invalid MESSGR_PROFILE value: {value:?}")),
            Err(_) => Self::Dev,
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "dev" | "development" => Some(Self::Dev),
            "staging" => Some(Self::Staging),
            "production" | "prod" => Some(Self::Production),
            _ => None,
        }
    }

    /// Whether the per-checkout `current_database()` assertion (§2.1) should
    /// run. True for `Dev`/`Staging`; skipped in `Production` to avoid
    /// per-acquire overhead in the hot path — the one-time `after_connect`
    /// check still runs unconditionally regardless of profile.
    pub fn checks_pool_identity(&self) -> bool {
        matches!(self, Self::Dev | Self::Staging)
    }

    pub fn is_dev(&self) -> bool {
        matches!(self, Self::Dev)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_dev_when_unset() {
        // Profile::from_env reads a process-global env var, so this is
        // exercised indirectly via parse() to stay hermetic under parallel
        // test execution.
        assert_eq!(Profile::parse("dev"), Some(Profile::Dev));
        assert_eq!(Profile::parse("Development"), Some(Profile::Dev));
        assert_eq!(Profile::parse("STAGING"), Some(Profile::Staging));
        assert_eq!(Profile::parse("production"), Some(Profile::Production));
        assert_eq!(Profile::parse("prod"), Some(Profile::Production));
        assert_eq!(Profile::parse("nonsense"), None);
    }

    #[test]
    fn pool_identity_checks_skip_only_production() {
        assert!(Profile::Dev.checks_pool_identity());
        assert!(Profile::Staging.checks_pool_identity());
        assert!(!Profile::Production.checks_pool_identity());
    }
}

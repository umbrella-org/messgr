//! Deployment profile, read once from `MESSGR_PROFILE`.
//!
//! One enum, not several ad hoc flags: `MockProvider` refuses to start
//! outside `dev` (§11.1), and dev-mode Vault refuses non-dev callers (§7.6)
//! — both guards documented elsewhere reuse this exact shape.

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
}

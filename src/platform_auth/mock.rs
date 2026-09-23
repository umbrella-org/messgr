use crate::profile::Profile;

use super::provider::{AuthError, PlatformAuthProvider, PlatformIdentity};

/// Local development and tests only (DESIGN.md §11.1) -- static identity
/// from config, no network. The guard lives in `new`, the only
/// constructor, so a `MockPlatformProvider` value can never exist outside
/// `profile = dev`, mirroring `auth::mock::MockProvider` exactly: enabling
/// this in production is a full authentication bypass onto a surface that
/// spans every tenant.
pub struct MockPlatformProvider {
    actor: String,
    role: String,
}

impl MockPlatformProvider {
    pub fn new(profile: Profile, actor: String, role: String) -> Self {
        if !profile.is_dev() {
            tracing::error!(
                "refusing to start: platform_auth.provider=mock while MESSGR_PROFILE is not \
                 dev (DESIGN.md \u{a7}11.1 -- a mock auth provider reachable in production is a \
                 full authentication bypass)"
            );
            panic!("MockPlatformProvider is not permitted outside profile=dev");
        }
        Self { actor, role }
    }
}

#[async_trait::async_trait]
impl PlatformAuthProvider for MockPlatformProvider {
    async fn authenticate(&self) -> Result<PlatformIdentity, AuthError> {
        Ok(PlatformIdentity {
            actor: self.actor.clone(),
            role: self.role.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(
        expected = "MockPlatformProvider is not permitted outside profile=dev"
    )]
    fn mock_provider_refuses_to_construct_outside_dev_profile() {
        MockPlatformProvider::new(
            Profile::Production,
            "actor".into(),
            "operator".into(),
        );
    }

    #[test]
    fn mock_provider_constructs_fine_in_dev_profile() {
        let _ =
            MockPlatformProvider::new(Profile::Dev, "actor".into(), "operator".into());
    }
}

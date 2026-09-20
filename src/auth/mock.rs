use uuid::Uuid;

use crate::profile::Profile;

use super::provider::{AuthError, AuthProvider, Identity};

/// Local development and tests only (DESIGN.md §11.1) -- static identity
/// from config, no network. The guard lives in `new`, the only
/// constructor, so a `MockProvider` value can never exist outside
/// `profile = dev`: enabling this in production is a full authentication
/// bypass.
pub struct MockProvider {
    actor: String,
    role: String,
}

impl MockProvider {
    pub fn new(profile: Profile, actor: String, role: String) -> Self {
        if !profile.is_dev() {
            tracing::error!(
                "refusing to start: auth.provider=mock while MESSGR_PROFILE is not dev \
                 (DESIGN.md \u{a7}11.1 -- a mock auth provider reachable in production is a \
                 full authentication bypass)"
            );
            panic!("MockProvider is not permitted outside profile=dev");
        }
        Self { actor, role }
    }
}

#[async_trait::async_trait]
impl AuthProvider for MockProvider {
    async fn authenticate(&self, tenant_id: Uuid) -> Result<Identity, AuthError> {
        Ok(Identity {
            actor: self.actor.clone(),
            role: self.role.clone(),
            tenant_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "MockProvider is not permitted outside profile=dev")]
    fn mock_provider_refuses_to_construct_outside_dev_profile() {
        MockProvider::new(Profile::Production, "actor".into(), "compliance".into());
    }

    #[test]
    fn mock_provider_constructs_fine_in_dev_profile() {
        let _ = MockProvider::new(Profile::Dev, "actor".into(), "compliance".into());
    }
}

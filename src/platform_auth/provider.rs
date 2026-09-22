pub use crate::auth::provider::AuthError;

/// The identity a platform-console request runs as (T-057, DESIGN.md
/// §11.4). Deliberately not `auth::provider::Identity`: that struct carries
/// a `tenant_id` because query-api requests are always scoped to one
/// tenant, which is the wrong shape for an operator identity spanning every
/// tenant by design.
#[derive(Debug, Clone)]
pub struct PlatformIdentity {
    /// Stable operator identifier, for the `platform_audit` trail.
    pub actor: String,
    /// One of the `role` module's constants.
    pub role: String,
}

/// Shaped so a future real provider (SSO for provider staff) slots in
/// behind the same trait without touching any call site, mirroring
/// `auth::provider::AuthProvider`'s own reasoning (§11.1).
#[async_trait::async_trait]
pub trait PlatformAuthProvider: Send + Sync {
    async fn authenticate(&self) -> Result<PlatformIdentity, AuthError>;
}

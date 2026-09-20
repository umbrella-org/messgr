use uuid::Uuid;

/// The identity a query-api request runs as, resolved by whichever
/// `AuthProvider` is configured (DESIGN.md §11.1, T-048 decision 1).
#[derive(Debug, Clone)]
pub struct Identity {
    /// Stable user identifier, for the access-audit log.
    pub actor: String,
    /// One of the `role` module's constants.
    pub role: String,
    pub tenant_id: Uuid,
}

#[derive(Debug)]
pub enum AuthError {
    Unauthenticated,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthenticated => write!(f, "unauthenticated"),
        }
    }
}

impl std::error::Error for AuthError {}

/// Shaped so a future `OidcProvider` (real redirect + session cookie,
/// reading the request itself) slots in behind the same trait without
/// touching any call site (T-048 decision 2). `OidcProvider` is a future
/// ticket — see T-048's Description.
#[async_trait::async_trait]
pub trait AuthProvider: Send + Sync {
    async fn authenticate(&self, tenant_id: Uuid) -> Result<Identity, AuthError>;
}

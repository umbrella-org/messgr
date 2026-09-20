use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};

use crate::auth::provider::{AuthError, Identity};

use super::AppState;
use super::tenant::{TenantContext, TenantContextError};

/// The authenticated identity for this request, resolved through whichever
/// `AuthProvider` is configured (T-048 decision 11) — tenant resolution
/// (`TenantContext`) always runs first, matching §11.1's ordering.
pub struct AuthedUser(pub Identity);

#[derive(Debug)]
pub enum AuthedUserError {
    Tenant(TenantContextError),
    Unauthenticated,
}

impl IntoResponse for AuthedUserError {
    fn into_response(self) -> Response {
        match self {
            Self::Tenant(err) => err.into_response(),
            Self::Unauthenticated => StatusCode::UNAUTHORIZED.into_response(),
        }
    }
}

impl FromRequestParts<AppState> for AuthedUser {
    type Rejection = AuthedUserError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let tenant_ctx = TenantContext::from_request_parts(parts, state)
            .await
            .map_err(AuthedUserError::Tenant)?;

        let identity = state
            .auth
            .authenticate(tenant_ctx.tenant_id)
            .await
            .map_err(|AuthError::Unauthenticated| AuthedUserError::Unauthenticated)?;

        Ok(AuthedUser(identity))
    }
}

/// A function, not a macro — three lines, called from every handler per the
/// role → endpoint matrix (T-048 decision 11/16). Returns `403` on mismatch.
pub fn require_role(identity: &Identity, allowed: &[&str]) -> Result<(), StatusCode> {
    if allowed.contains(&identity.role.as_str()) {
        Ok(())
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}

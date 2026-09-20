use std::collections::HashMap;

use axum::extract::{FromRequestParts, Path};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use sqlx::PgPool;
use uuid::Uuid;

use crate::tenant::repo as tenant_repo;

use super::AppState;

/// Resolves the `:tenant_slug` path segment to a tenant, then opens (or
/// reuses) that tenant's pool — before `AuthProvider::authenticate` runs,
/// matching §11.1's "tenant resolution happens... before the OIDC flow
/// starts" (T-048 decision 3).
pub struct TenantContext {
    pub tenant_id: Uuid,
    pub pool: PgPool,
    /// Carried from the same `tenant` row lookup this extractor already
    /// makes — needed for message-detail decryption (T-048 decision 14).
    pub vault_mount: String,
}

#[derive(Debug)]
pub enum TenantContextError {
    UnknownTenant,
    Database(sqlx::Error),
}

impl IntoResponse for TenantContextError {
    fn into_response(self) -> Response {
        match self {
            Self::UnknownTenant => StatusCode::NOT_FOUND.into_response(),
            Self::Database(err) => {
                tracing::error!(%err, "query-api: database error resolving tenant context");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
    }
}

impl FromRequestParts<AppState> for TenantContext {
    type Rejection = TenantContextError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let params = Path::<HashMap<String, String>>::from_request_parts(parts, state)
            .await
            .map_err(|_| TenantContextError::UnknownTenant)?;
        let slug = params
            .get("tenant_slug")
            .ok_or(TenantContextError::UnknownTenant)?;

        let tenant = tenant_repo::find_by_slug(&state.control_pool, slug)
            .await
            .map_err(TenantContextError::Database)?
            .ok_or(TenantContextError::UnknownTenant)?;

        let pool = state
            .pool_cache
            .get_or_open(
                &state.control_pool,
                &state.query_api_database_url,
                tenant.id,
                &tenant.database_name,
                state.tenant_pool_max_connections,
            )
            .await
            .map_err(TenantContextError::Database)?;

        Ok(TenantContext {
            tenant_id: tenant.id,
            pool,
            vault_mount: tenant.vault_mount,
        })
    }
}

//! The platform console (T-057, DESIGN.md §11.4): a `serve` subcommand on
//! `messgr-control`, for provider staff, in a separate authentication realm
//! from the tenant admin panel (`messgr-query-api`, T-049). This app state
//! holds a `control_pool` only, never Vault Transit credentials -- there is
//! no code path by which this binary could reach a tenant's Transit mount
//! or decrypt a payload (decision 4).

pub mod audit;
pub mod health;
pub mod kill_switches;
pub mod tenants;

use std::sync::Arc;

use askama::Template;
use axum::Router;
use axum::body::Bytes;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use sqlx::PgPool;

use crate::platform_auth::provider::{
    AuthError, PlatformAuthProvider, PlatformIdentity,
};

/// Shared state for every platform-console request handler.
#[derive(Clone)]
pub struct AppState {
    pub control_pool: PgPool,
    /// Base connection string the T-028 health query swaps a tenant's
    /// database name into (decision 4) -- a short-lived, metadata-only
    /// tenant-database connection, not Vault Transit access.
    pub base_db_url: String,
    pub auth: Arc<dyn PlatformAuthProvider>,
}

pub(crate) fn render<T: Template>(template: T) -> Response {
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(err) => {
            tracing::error!(%err, "platform-console: template render failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// The authenticated operator identity for this request, resolved through
/// whichever `PlatformAuthProvider` is configured -- mirrors query-api's
/// `AuthedUser`, minus the tenant-resolution step this surface has none of.
pub struct PlatformAuthedUser(pub PlatformIdentity);

impl FromRequestParts<AppState> for PlatformAuthedUser {
    type Rejection = StatusCode;

    async fn from_request_parts(
        _parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        state
            .auth
            .authenticate()
            .await
            .map(PlatformAuthedUser)
            .map_err(|AuthError::Unauthenticated| StatusCode::UNAUTHORIZED)
    }
}

/// A function, not a macro -- mirrors `query_api::auth_mw::require_role`.
pub(crate) fn require_role(
    identity: &PlatformIdentity,
    allowed: &[&str],
) -> Result<(), StatusCode> {
    if allowed.contains(&identity.role.as_str()) {
        Ok(())
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}

/// Vendored, not CDN-loaded, same `include_bytes!` pattern and the same
/// asset `query_api::handlers::datastar_asset` vendors, for this binary's
/// own route tree.
async fn datastar_asset() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/javascript")],
        Bytes::from_static(include_bytes!("../../assets/datastar.js")),
    )
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/ui/tenants", get(tenants::ui_tenants))
        .route("/tenants/{id}/suspend", post(tenants::suspend))
        .route("/ui/health", get(health::ui_health))
        .route("/ui/audit", get(audit::ui_audit))
        .route("/ui/kill-switches", get(kill_switches::ui_kill_switches))
        .route("/assets/datastar.js", get(datastar_asset))
        .with_state(state)
}

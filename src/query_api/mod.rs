pub mod auth_mw;
pub mod handlers;
pub mod path;
pub mod tenant;
pub mod views;

use std::sync::Arc;

use axum::Router;
use axum::routing::get;
use sqlx::PgPool;

use crate::auth::provider::AuthProvider;
use crate::keystore::KeyStore;
use crate::webhook::TenantPoolCache;

/// Shared state for every `messgr-query-api` request handler
/// (`axum::extract::State`).
#[derive(Clone)]
pub struct AppState {
    pub control_pool: PgPool,
    pub query_api_database_url: String,
    pub auth: Arc<dyn AuthProvider>,
    pub pool_cache: Arc<TenantPoolCache>,
    pub tenant_pool_max_connections: u32,
    /// Vault Transit unwrap capability (T-048 decision 14) — read-only,
    /// scoped to whatever policy each tenant's AppRole carries. Needed only
    /// for message-detail body decryption.
    pub keystore: Arc<dyn KeyStore>,
}

/// Assembles the full router: the six read-only data routes plus their
/// UI counterparts, all nested under `/t/{tenant_slug}`, each wrapped with
/// the compliance access-audit middleware (decision 17); the OpenAPI
/// document and vendored htmx asset are mounted unauthenticated.
pub fn router(state: AppState) -> Router {
    let data_routes = Router::new()
        .route("/comms", get(handlers::list_comms))
        .route("/comms/{id}", get(handlers::comms_detail))
        .route("/customers/{id}/timeline", get(handlers::customer_timeline))
        .route("/campaigns/{id}/reach", get(handlers::campaign_reach))
        .route("/producers/{id}/usage", get(handlers::producer_usage))
        .route("/producers/{id}/quota", get(handlers::producer_quota))
        .route(
            "/ui/customers/{id}/timeline",
            get(handlers::ui_customer_timeline),
        )
        .route("/ui/comms/{id}", get(handlers::ui_comms_detail))
        .route("/ui/campaigns/{id}/reach", get(handlers::ui_campaign_reach))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            handlers::access_audit_mw,
        ));

    let tenant_nest = Router::new()
        .merge(data_routes)
        .route("/openapi.yaml", get(handlers::openapi_spec));

    Router::new()
        .nest("/t/{tenant_slug}", tenant_nest)
        .route("/assets/htmx.min.js", get(handlers::htmx_asset))
        .with_state(state)
}

pub mod admin;
pub mod auth_mw;
pub mod handlers;
pub mod path;
pub mod tenant;
pub mod views;

use std::sync::Arc;

use askama::Template;
use axum::Router;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use sqlx::PgPool;

use crate::auth::provider::AuthProvider;
use crate::keystore::KeyStore;
use crate::webhook::TenantPoolCache;

/// Shared by `views.rs` (T-048) and `admin.rs` (T-049) — every server-
/// rendered `GET` handler in this binary renders its Askama template the
/// same way.
pub(crate) fn render<T: Template>(template: T) -> Response {
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(err) => {
            tracing::error!(%err, "query-api: template render failed");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

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

/// Assembles the full router: the six read-only data routes and the T-049
/// admin panel's routes, plus their UI counterparts, all nested under
/// `/t/{tenant_slug}`, each wrapped with the compliance access-audit
/// middleware (decision 17); the OpenAPI document and vendored htmx/Datastar
/// assets are mounted unauthenticated.
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
        // Admin panel (T-049) — operator UI only, no OpenAPI entries.
        .route("/ui/admin/quota", get(admin::ui_quota_dashboard))
        .route("/ui/admin/kill-switches", get(admin::ui_kill_switches))
        .route(
            "/ui/admin/kill-switches/blast-radius",
            get(admin::ui_blast_radius),
        )
        .route("/admin/kill-switches", post(admin::engage_kill_switch))
        .route(
            "/admin/kill-switches/{id}/release",
            post(admin::release_kill_switch),
        )
        .route("/ui/admin/scheduled", get(admin::ui_scheduled_queue))
        .route(
            "/admin/scheduled/{id}/cancel",
            post(admin::cancel_scheduled),
        )
        .route("/ui/admin/producers", get(admin::ui_producer_registry))
        .route("/admin/producers", post(admin::register_producer))
        .route(
            "/admin/producers/{id}/disable",
            post(admin::disable_producer),
        )
        .route(
            "/admin/producers/{id}/quota",
            post(admin::set_producer_quota),
        )
        .route(
            "/admin/producers/{id}/quota/override",
            post(admin::add_quota_override),
        )
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
        .route("/assets/datastar.js", get(handlers::datastar_asset))
        .with_state(state)
}

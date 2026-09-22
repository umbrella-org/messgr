//! The `platform_audit` trail (T-057): paginated, newest first, filterable
//! by tenant id / action.

use askama::Template;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use uuid::Uuid;

use crate::platform_audit::{self, PlatformAuditRow};

use super::{AppState, PlatformAuthedUser, render, require_role};
use crate::platform_auth::role;

/// First-cut fixed page size, mirroring `query_api::admin::SCHEDULED_LIMIT`
/// -- no pagination scheme beyond page number specified for this surface
/// either.
const PAGE_SIZE: i64 = 50;

#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    tenant_id: Option<Uuid>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    page: Option<i64>,
}

#[derive(Template)]
#[template(path = "platform_console/audit.html")]
struct AuditTemplate {
    actor: String,
    role: String,
    filter_tenant_id: String,
    filter_action: String,
    page: i64,
    rows: Vec<PlatformAuditRow>,
}

pub async fn ui_audit(
    PlatformAuthedUser(identity): PlatformAuthedUser,
    State(state): State<AppState>,
    Query(query): Query<AuditQuery>,
) -> Response {
    if let Err(status) = require_role(&identity, &[role::OPERATOR]) {
        return status.into_response();
    }

    let page = query.page.unwrap_or(1).max(1);
    let action = query.action.filter(|s| !s.is_empty());

    let rows = match platform_audit::list(
        &state.control_pool,
        query.tenant_id,
        action.as_deref(),
        PAGE_SIZE,
        (page - 1) * PAGE_SIZE,
    )
    .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(%err, "platform-console: failed to list platform_audit");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    render(AuditTemplate {
        actor: identity.actor,
        role: identity.role,
        filter_tenant_id: query.tenant_id.map(|id| id.to_string()).unwrap_or_default(),
        filter_action: action.unwrap_or_default(),
        page,
        rows,
    })
}

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
    #[serde(default, deserialize_with = "empty_uuid_as_none")]
    tenant_id: Option<Uuid>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    page: Option<i64>,
}

/// Mirrors `query_api::admin::empty_uuid_as_none` -- the audit filter form
/// sends `tenant_id=` (present, empty) whenever the tenant box is blank and
/// any other filter changes, and `Uuid`'s own `Deserialize` rejects "".
fn empty_uuid_as_none<'de, D>(deserializer: D) -> Result<Option<Uuid>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    match opt.as_deref() {
        None | Some("") => Ok(None),
        Some(s) => Uuid::parse_str(s)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

#[derive(Template)]
#[template(path = "platform_console/audit.html")]
struct AuditTemplate {
    actor: String,
    role: String,
    filter_tenant_id: String,
    filter_action: String,
    /// The `data-signals` init value, pre-encoded as JSON. Datastar parses
    /// this attribute as a JS expression, so handing it raw
    /// `'{{ filter_action }}'`-style string literals lets a value like
    /// `x'});fetch(...)});({y='` break out of the quotes once the browser
    /// HTML-decodes the attribute -- Askama's escaping protects the HTML
    /// parse, not the JS parse that follows it. `serde_json` escapes quotes
    /// and backslashes for the JS/JSON layer; Askama's own HTML-escaping of
    /// the resulting `"`s (`&quot;`) then protects the HTML layer on top,
    /// and the browser undoes exactly that one layer before Datastar sees
    /// valid JSON, never attacker-controlled JS syntax.
    signals: String,
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

    let filter_tenant_id = query.tenant_id.map(|id| id.to_string()).unwrap_or_default();
    let filter_action = action.unwrap_or_default();
    let signals = serde_json::json!({
        "tenantId": filter_tenant_id,
        "action": filter_action,
    })
    .to_string();

    render(AuditTemplate {
        actor: identity.actor,
        role: identity.role,
        filter_tenant_id,
        filter_action,
        signals,
        page,
        rows,
    })
}

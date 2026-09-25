//! Platform kill switches pane (T-058, DESIGN.md §5.2 "Two tiers in cloud",
//! §11.4): the active platform-tier switches, an engage form, and a
//! release action per switch. This console is the only UI that can release
//! a platform switch -- the tenant admin panel shows one as non-actionable
//! (T-058 decision 4).

use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

use super::{AppState, PlatformAuthedUser, render, require_role};
use crate::platform_auth::role;
use crate::platform_kill_switch::configure::{self, ConfigureError};
use crate::platform_kill_switch::model::scope;
use crate::platform_kill_switch::repo as platform_repo;
use crate::tenant::model::Tenant;
use crate::tenant::repo as tenant_repo;

#[derive(Template)]
#[template(path = "platform_console/kill_switches.html")]
struct KillSwitchesTemplate {
    actor: String,
    role: String,
    rows: Vec<SwitchRow>,
    tenants: Vec<Tenant>,
    /// The last engage/release's outcome, shown above the list -- `None` on
    /// a plain page load.
    message: Option<String>,
}

struct SwitchRow {
    id: Uuid,
    scope: String,
    /// The tenant's slug for a tenant-scope switch, "all tenants" for a
    /// region-wide one.
    target: String,
    engaged_by: String,
    engaged_at: DateTime<Utc>,
    reason: String,
}

async fn kill_switches_page(
    state: &AppState,
    actor: String,
    role: String,
    message: Option<String>,
) -> Response {
    let active = match platform_repo::list_active(&state.control_pool).await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(%err, "platform-console: failed to list platform kill switches");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let tenants = match tenant_repo::list(&state.control_pool).await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(%err, "platform-console: failed to list tenants");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let rows = active
        .into_iter()
        .map(|switch| {
            let target = match switch.tenant_id {
                Some(id) => tenants
                    .iter()
                    .find(|t| t.id == id)
                    .map(|t| t.slug.clone())
                    .unwrap_or_else(|| id.to_string()),
                None => "all tenants".to_string(),
            };
            SwitchRow {
                id: switch.id,
                scope: switch.scope,
                target,
                engaged_by: switch.engaged_by,
                engaged_at: switch.engaged_at,
                reason: switch.reason,
            }
        })
        .collect();

    render(KillSwitchesTemplate {
        actor,
        role,
        rows,
        tenants,
        message,
    })
}

pub async fn ui_kill_switches(
    PlatformAuthedUser(identity): PlatformAuthedUser,
    State(state): State<AppState>,
) -> Response {
    if let Err(status) = require_role(&identity, &[role::OPERATOR]) {
        return status.into_response();
    }
    kill_switches_page(&state, identity.actor, identity.role, None).await
}

#[derive(Debug, Deserialize)]
pub struct EngageForm {
    scope: String,
    /// Required for `scope = tenant`, ignored for `scope = platform` -- the
    /// form always sends the select's value, so a region-wide engage must
    /// not be rejected for carrying one.
    #[serde(default)]
    tenant_id: Option<String>,
    reason: String,
}

pub async fn engage(
    PlatformAuthedUser(identity): PlatformAuthedUser,
    State(state): State<AppState>,
    Form(form): Form<EngageForm>,
) -> Response {
    if let Err(status) = require_role(&identity, &[role::OPERATOR]) {
        return status.into_response();
    }
    if form.reason.trim().is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }

    let tenant_id = if form.scope == scope::TENANT {
        match form.tenant_id.as_deref().map(Uuid::parse_str) {
            Some(Ok(id)) => Some(id),
            _ => return StatusCode::BAD_REQUEST.into_response(),
        }
    } else {
        None
    };

    let message = match configure::engage(
        &state.control_pool,
        &state.base_db_url,
        &form.scope,
        tenant_id,
        &form.reason,
        &identity.actor,
    )
    .await
    {
        Ok((_, report)) => fan_out_message("Engaged", report),
        Err(ConfigureError::AlreadyEngaged) => {
            "A switch with this scope is already engaged.".to_string()
        }
        Err(ConfigureError::Rejected(message)) => {
            tracing::warn!(%message, "platform-console: platform kill switch engage rejected");
            return StatusCode::BAD_REQUEST.into_response();
        }
        Err(ConfigureError::Database(err)) => {
            tracing::error!(%err, "platform-console: failed to engage platform kill switch");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    kill_switches_page(&state, identity.actor, identity.role, Some(message)).await
}

pub async fn release(
    PlatformAuthedUser(identity): PlatformAuthedUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Response {
    if let Err(status) = require_role(&identity, &[role::OPERATOR]) {
        return status.into_response();
    }

    let message = match configure::release(
        &state.control_pool,
        &state.base_db_url,
        id,
        &identity.actor,
    )
    .await
    {
        Ok((outcome, report)) if outcome.outcome == "released" => {
            fan_out_message("Released", report)
        }
        Ok(_) => "Already released.".to_string(),
        Err(err) => {
            tracing::error!(%err, switch_id = %id, "platform-console: failed to release platform kill switch");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    kill_switches_page(&state, identity.actor, identity.role, Some(message)).await
}

fn fan_out_message(verb: &str, report: configure::FanOutReport) -> String {
    if report.failed == 0 {
        format!("{verb}; notified {} tenant database(s).", report.notified)
    } else {
        format!(
            "{verb}; notified {} tenant database(s), {} unreachable -- those pick the \
             change up on their next 30-second poll.",
            report.notified, report.failed
        )
    }
}

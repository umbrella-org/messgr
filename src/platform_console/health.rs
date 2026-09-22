//! Per-tenant health/volume view (T-057), reusing T-028's
//! `stats::tenant_message_stats` query unmodified -- the same counts
//! `messgr-control stats` already returns for the same tenant.

use askama::Template;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::stats::{self, ChannelStatusCount};
use crate::tenant::repo as tenant_repo;

use super::{AppState, PlatformAuthedUser, render, require_role};
use crate::platform_auth::role;

#[derive(Template)]
#[template(path = "platform_console/health.html")]
struct HealthTemplate {
    actor: String,
    role: String,
    tenants: Vec<TenantHealth>,
}

struct TenantHealth {
    slug: String,
    counts: Vec<ChannelStatusCount>,
    error: Option<String>,
}

pub async fn ui_health(
    PlatformAuthedUser(identity): PlatformAuthedUser,
    State(state): State<AppState>,
) -> Response {
    if let Err(status) = require_role(&identity, &[role::OPERATOR]) {
        return status.into_response();
    }

    let tenants = match tenant_repo::list(&state.control_pool).await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(%err, "platform-console: failed to list tenants for health view");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let mut rows = Vec::with_capacity(tenants.len());
    for tenant in tenants {
        let result = stats::tenant_message_stats(
            &state.control_pool,
            &state.base_db_url,
            &tenant.slug,
            None,
        )
        .await;
        rows.push(match result {
            Ok(counts) => TenantHealth {
                slug: tenant.slug,
                counts,
                error: None,
            },
            Err(err) => TenantHealth {
                slug: tenant.slug,
                counts: Vec::new(),
                error: Some(err.to_string()),
            },
        });
    }

    render(HealthTemplate {
        actor: identity.actor,
        role: identity.role,
        tenants: rows,
    })
}

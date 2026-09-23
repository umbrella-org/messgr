//! Per-tenant health/volume view (T-057), reusing T-028's
//! `stats::tenant_message_stats` query unmodified -- the same counts
//! `messgr-control stats` already returns for the same tenant.

use askama::Template;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use tokio::task::JoinSet;

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

    // Independent per-tenant lookups, each opening its own pool
    // (`stats::tenant_message_stats` -> `connect_tenant_pool`) -- run them
    // concurrently instead of serially so page latency is bounded by the
    // slowest tenant, not their sum.
    let mut set = JoinSet::new();
    for (idx, tenant) in tenants.into_iter().enumerate() {
        let control_pool = state.control_pool.clone();
        let base_db_url = state.base_db_url.clone();
        set.spawn(async move {
            let result = stats::tenant_message_stats(
                &control_pool,
                &base_db_url,
                &tenant.slug,
                None,
            )
            .await;
            (idx, tenant.slug, result)
        });
    }

    let mut rows = Vec::with_capacity(set.len());
    while let Some(joined) = set.join_next().await {
        let (idx, slug, result) = joined.expect("tenant health stats task panicked");
        let health = match result {
            Ok(counts) => TenantHealth {
                slug,
                counts,
                error: None,
            },
            Err(err) => TenantHealth {
                slug,
                counts: Vec::new(),
                error: Some(err.to_string()),
            },
        };
        rows.push((idx, health));
    }
    rows.sort_by_key(|(idx, _)| *idx);
    let rows = rows.into_iter().map(|(_, health)| health).collect();

    render(HealthTemplate {
        actor: identity.actor,
        role: identity.role,
        tenants: rows,
    })
}

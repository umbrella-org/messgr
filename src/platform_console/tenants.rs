//! Tenant lifecycle view (T-057): list, `tenant_schema_version` drift,
//! suspend. Offboard-trigger is deliberately not a route here (decision
//! 4a) -- `tenant::offboard::destroy_tenant` needs a `VaultClient`, which
//! this console's app state must never hold.

use askama::Template;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::tenant::model::status;
use crate::tenant::repo as tenant_repo;

use super::{AppState, PlatformAuthedUser, render, require_role};
use crate::platform_auth::role;

#[derive(Template)]
#[template(path = "platform_console/tenants.html")]
struct TenantsTemplate {
    actor: String,
    role: String,
    rows: Vec<TenantRow>,
}

struct TenantRow {
    id: Uuid,
    slug: String,
    region: String,
    status: String,
    schema_version: String,
    created_at: DateTime<Utc>,
}

async fn tenants_page(pool: &sqlx::PgPool, actor: String, role: String) -> Response {
    let tenants = match tenant_repo::list(pool).await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(%err, "platform-console: failed to list tenants");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let versions = match tenant_repo::schema_versions(pool).await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(%err, "platform-console: failed to list schema versions");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let rows = tenants
        .into_iter()
        .map(|tenant| {
            let schema_version = versions
                .iter()
                .find(|(id, _)| *id == tenant.id)
                .map(|(_, version)| version.to_string())
                .unwrap_or_else(|| "-".to_string());
            TenantRow {
                id: tenant.id,
                slug: tenant.slug,
                region: tenant.region,
                status: tenant.status,
                schema_version,
                created_at: tenant.created_at,
            }
        })
        .collect();

    render(TenantsTemplate { actor, role, rows })
}

pub async fn ui_tenants(
    PlatformAuthedUser(identity): PlatformAuthedUser,
    State(state): State<AppState>,
) -> Response {
    if let Err(status) = require_role(&identity, &[role::OPERATOR]) {
        return status.into_response();
    }
    tenants_page(&state.control_pool, identity.actor, identity.role).await
}

pub async fn suspend(
    PlatformAuthedUser(identity): PlatformAuthedUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Response {
    if let Err(status) = require_role(&identity, &[role::OPERATOR]) {
        return status.into_response();
    }

    let mut tx = match state.control_pool.begin().await {
        Ok(tx) => tx,
        Err(err) => {
            tracing::error!(%err, tenant_id = %id, "platform-console: failed to start suspend transaction");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let rows_affected = match tenant_repo::mark_status_tx(
        &mut tx,
        id,
        status::SUSPENDED,
    )
    .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(%err, tenant_id = %id, "platform-console: failed to suspend tenant");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    if rows_affected == 0 {
        return StatusCode::NOT_FOUND.into_response();
    }

    if let Err(err) = crate::platform_audit::record_tx(
        &mut tx,
        &identity.actor,
        "tenant.suspend",
        Some(id),
        serde_json::json!({}),
    )
    .await
    {
        tracing::error!(%err, tenant_id = %id, "platform-console: failed to write platform_audit row for suspend");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    if let Err(err) = tx.commit().await {
        tracing::error!(%err, tenant_id = %id, "platform-console: failed to commit suspend transaction");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    tenants_page(&state.control_pool, identity.actor, identity.role).await
}

use askama::Template;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use uuid::Uuid;

use crate::auth::role;
use crate::comms_query::model::{CampaignReachCount, CommsRequestSummary};
use crate::comms_query::repo as comms_repo;

use super::AppState;
use super::auth_mw::{AuthedUser, require_role};
use super::handlers::{database_error, decrypt_detail, identity_owns};
use super::path::{CampaignIdPath, IdPath};
use super::render;
use super::tenant::TenantContext;

#[derive(Template)]
#[template(path = "query_api/timeline.html")]
struct TimelineTemplate {
    actor: String,
    role: String,
    customer_id: Uuid,
    rows: Vec<CommsRequestSummary>,
}

pub async fn ui_customer_timeline(
    AuthedUser(identity): AuthedUser,
    tenant: TenantContext,
    Path(IdPath { id: customer_id }): Path<IdPath>,
) -> Response {
    match identity.role.as_str() {
        role::COMPLIANCE => {}
        role::CUSTOMER_SERVICE if identity_owns(&identity, customer_id) => {}
        _ => return StatusCode::FORBIDDEN.into_response(),
    }

    match comms_repo::timeline(&tenant.pool, customer_id, 200).await {
        Ok(rows) => render(TimelineTemplate {
            actor: identity.actor,
            role: identity.role,
            customer_id,
            rows,
        }),
        Err(err) => database_error(err),
    }
}

#[derive(Template)]
#[template(path = "query_api/message_detail.html")]
struct MessageDetailTemplate {
    actor: String,
    role: String,
    detail: crate::comms_query::model::CommsRequestDetailView,
    events: Vec<crate::comms_query::model::CommsEventRow>,
}

#[derive(serde::Deserialize)]
pub struct MessageDetailQuery {
    created_at: chrono::DateTime<chrono::Utc>,
}

pub async fn ui_comms_detail(
    AuthedUser(identity): AuthedUser,
    tenant: TenantContext,
    Path(IdPath { id }): Path<IdPath>,
    axum::extract::Query(query): axum::extract::Query<MessageDetailQuery>,
    State(state): State<AppState>,
) -> Response {
    let row = match comms_repo::detail(&tenant.pool, query.created_at, id).await {
        Ok(Some(row)) => row,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => return database_error(err),
    };

    match identity.role.as_str() {
        role::COMPLIANCE => {}
        role::CUSTOMER_SERVICE if identity_owns(&identity, row.customer_id) => {}
        _ => return StatusCode::FORBIDDEN.into_response(),
    }

    let events = match comms_repo::events(&tenant.pool, id).await {
        Ok(events) => events,
        Err(err) => return database_error(err),
    };

    match decrypt_detail(&state, &tenant, row).await {
        Ok(detail) => render(MessageDetailTemplate {
            actor: identity.actor,
            role: identity.role,
            detail,
            events,
        }),
        Err(status) => status.into_response(),
    }
}

#[derive(Template)]
#[template(path = "query_api/campaign_reach.html")]
struct CampaignReachTemplate {
    actor: String,
    role: String,
    campaign_id: String,
    counts: Vec<CampaignReachCount>,
}

pub async fn ui_campaign_reach(
    AuthedUser(identity): AuthedUser,
    tenant: TenantContext,
    Path(CampaignIdPath { id: campaign_id }): Path<CampaignIdPath>,
) -> Response {
    if let Err(status) =
        require_role(&identity, &[role::COMPLIANCE, role::CAMPAIGN_OPS])
    {
        return status.into_response();
    }

    match comms_repo::campaign_reach(&tenant.pool, &campaign_id).await {
        Ok(counts) => render(CampaignReachTemplate {
            actor: identity.actor,
            role: identity.role,
            campaign_id,
            counts,
        }),
        Err(err) => database_error(err),
    }
}

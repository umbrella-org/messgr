//! `POST /webhook/:webhook_token/:provider` (T-047, DESIGN.md §10): resolve
//! the tenant by its opaque `webhook_token`, verify the raw body's
//! signature, normalize the payload, and write one row to
//! `webhook_receipt_staging` -- the only SQL statement this binary ever
//! runs. Encryption and promotion into `comms_event`/`orphan_event` happen
//! later, out of the DMZ, via `messgr-control webhook-promote` (T-047
//! decision 1).

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

use crate::tenant::repo as tenant_repo;
use crate::webhook_receipt::repo as webhook_receipt_repo;

use super::AppState;

/// The header a provider's signature arrives on. Generic-provider
/// convention (T-047 decision 2) -- a real vendor's own header name is
/// follow-up work for whenever a provider is chosen.
const SIGNATURE_HEADER: &str = "x-webhook-signature";

/// The generic inbound receipt shape (mirrors `sender::http::HttpSender`'s
/// own generic outbound contract, T-012 decision 1, rather than any one
/// real vendor's webhook body).
#[derive(Debug, Deserialize)]
struct GenericReceipt {
    provider_ref: String,
    event_type: String,
    occurred_at: DateTime<Utc>,
    provider_status: Option<String>,
}

pub async fn receive_webhook(
    State(state): State<AppState>,
    Path((webhook_token, provider)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    // Unknown token -> a flat 404, identical in shape to any other
    // unmatched route: the response must not distinguish "unknown token"
    // from "no such path" for an unauthenticated caller (§10). A lookup
    // error is treated the same way -- fail closed, never leak more than a
    // 404 before the caller is authenticated.
    let tenant = match tenant_repo::find_by_webhook_token(
        &state.control_pool,
        &webhook_token,
    )
    .await
    {
        Ok(Some(tenant)) => tenant,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => {
            tracing::error!(%err, "messgr-webhook: tenant lookup by webhook_token failed");
            return StatusCode::NOT_FOUND.into_response();
        }
    };

    let signature_header = headers
        .get(SIGNATURE_HEADER)
        .and_then(|value| value.to_str().ok());

    let secret = match state.vault_keystore.read_webhook_secret(&tenant.slug).await {
        Ok(secret) => secret,
        Err(err) => {
            tracing::error!(
                tenant_slug = %tenant.slug,
                %err,
                "messgr-webhook: reading webhook secret from Vault failed"
            );
            return StatusCode::UNAUTHORIZED.into_response();
        }
    };

    let verified = signature_header
        .is_some_and(|header| state.verifier.verify(&secret, &body, header));
    if !verified {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let raw: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(raw) => raw,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let receipt: GenericReceipt = match serde_json::from_value(raw.clone()) {
        Ok(receipt) => receipt,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    let tenant_pool = match state
        .pool_cache
        .get_or_open(
            &state.control_pool,
            &state.control_database_url,
            tenant.id,
            &tenant.database_name,
            state.tenant_pool_max_connections,
        )
        .await
    {
        Ok(pool) => pool,
        Err(err) => {
            tracing::error!(tenant_slug = %tenant.slug, %err, "messgr-webhook: opening tenant pool failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let result = webhook_receipt_repo::insert_staging(
        &tenant_pool,
        Uuid::new_v4(),
        Utc::now(),
        &provider,
        &receipt.provider_ref,
        receipt.occurred_at,
        &receipt.event_type,
        receipt.provider_status.as_deref(),
        &raw,
    )
    .await;

    match result {
        Ok(()) => StatusCode::OK.into_response(),
        Err(err) => {
            tracing::error!(tenant_slug = %tenant.slug, %err, "messgr-webhook: writing webhook_receipt_staging failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

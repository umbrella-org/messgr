use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use crate::mtls::PeerCertSubject;
use crate::producer::resolve::resolve_producer;
use crate::tenant::registry::TenantContext;

use super::AppState;
use super::model::IngestError;

/// An authenticated producer, resolved from the mTLS layer's verified peer
/// certificate — never from the request body (DESIGN.md §4.9, §11.1;
/// AGENTS.md hard invariant 10).
pub struct ProducerContext {
    pub producer_id: uuid::Uuid,
    pub tenant: Arc<TenantContext>,
}

impl FromRequestParts<AppState> for ProducerContext {
    type Rejection = IngestError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let subject = parts
            .extensions
            .get::<PeerCertSubject>()
            .ok_or(IngestError::MissingPeerCertificate)?
            .0
            .clone();

        let identity = resolve_producer(&state.control_pool, &subject).await?;

        let tenant = state
            .registry
            .get_or_open(
                &state.control_pool,
                &state.control_database_url,
                &*state.keystore,
                identity.tenant_id,
                state.tenant_pool_max_connections,
                state.profile,
            )
            .await?;

        Ok(ProducerContext {
            producer_id: identity.producer_id,
            tenant,
        })
    }
}

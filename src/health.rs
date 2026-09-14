//! Plain, unauthenticated liveness endpoint (T-032): "the process is up and
//! serving", not readiness against DB/Vault/downstream dependencies. Shared
//! by `messgr-ingest` and `messgr-dispatcher`, each binding it on its own
//! second, non-TLS listener alongside its existing server/loop.

use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;

pub fn router() -> Router {
    Router::new().route("/healthz", get(healthz))
}

async fn healthz() -> StatusCode {
    StatusCode::OK
}

mod handlers;
pub mod model;
pub mod repo;
pub mod worker;

use axum::{Router, routing::post};

use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/send/sms", post(handlers::create).get(handlers::list))
}

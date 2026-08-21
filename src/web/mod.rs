mod handlers;
mod html;

use axum::{Router, routing::get};

use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(handlers::index))
        .route("/sms/search", get(handlers::search))
        .route("/static/style.css", get(handlers::style))
}

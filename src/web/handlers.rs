use axum::{
    extract::{Query, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse},
};
use serde::Deserialize;

use super::html;
use crate::sms::repo;
use crate::state::AppState;

pub async fn index() -> impl IntoResponse {
    Html(html::page())
}

pub async fn style() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        html::STYLE,
    )
}

const PAGE_SIZE: i64 = 20;

#[derive(Debug, Deserialize)]
pub struct SearchParams {
    pub q: Option<String>,
    pub page: Option<u32>,
}

pub async fn search(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> impl IntoResponse {
    let query = params.q.unwrap_or_default();
    let page = params.page.unwrap_or(1).max(1);
    let offset = i64::from(page - 1) * PAGE_SIZE;

    match repo::search(&state.pool, &query, PAGE_SIZE + 1, offset).await {
        Ok(mut messages) => {
            let has_next = messages.len() as i64 > PAGE_SIZE;
            messages.truncate(PAGE_SIZE as usize);
            Html(html::results(&messages, page, has_next)).into_response()
        }
        Err(err) => {
            tracing::error!(?err, "failed to search sms messages");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

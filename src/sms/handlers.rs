use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use chrono::Utc;
use tokio::sync::mpsc::error::TrySendError;
use uuid::Uuid;

use crate::state::AppState;

use super::{
    model::{CreateSmsMessage, SmsMessage},
    repo,
};

pub async fn create(
    State(state): State<AppState>,
    Json(payload): Json<CreateSmsMessage>,
) -> impl IntoResponse {
    let message = SmsMessage {
        id: Uuid::new_v4(),
        sender: payload.sender,
        recipient: payload.recipient,
        body: payload.body,
        received_at: Utc::now(),
    };

    let response_body = message.clone();

    match state.sms_tx.try_send(message) {
        Ok(()) => (StatusCode::CREATED, Json(response_body)).into_response(),
        Err(TrySendError::Full(_)) => {
            tracing::error!("sms queue is full, rejecting incoming message");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
        Err(TrySendError::Closed(_)) => {
            tracing::error!("sms queue is closed, rejecting incoming message");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

pub async fn list(State(state): State<AppState>) -> impl IntoResponse {
    match repo::list(&state.pool).await {
        Ok(messages) => (StatusCode::OK, Json(messages)).into_response(),
        Err(err) => {
            tracing::error!(?err, "failed to list sms messages");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

//! Platform kill switches pane (T-058, soft coupling per T-057's
//! Description) -- stub until T-058 lands. No template file: this is a
//! fixed "not yet available" response, not a view over data that doesn't
//! exist yet.

use axum::extract::State;
use axum::response::{Html, IntoResponse, Response};

use super::{AppState, PlatformAuthedUser, require_role};
use crate::platform_auth::role;

pub async fn ui_kill_switches(
    PlatformAuthedUser(identity): PlatformAuthedUser,
    State(_state): State<AppState>,
) -> Response {
    if let Err(status) = require_role(&identity, &[role::OPERATOR]) {
        return status.into_response();
    }
    Html("<p>Platform kill switches: not yet available (T-058).</p>").into_response()
}

//! Auth HTTP surface (password mode): login, logout, setup, whoami.
//! These are reeve surfaces (spec/reeve/01-framework.md §3.1 rule 4) —
//! nothing here shadows a Margo path.

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use device_api::{Identity, Role, token_hash};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::warn;

use super::{sessions, users};
use crate::config::AuthMode;
use crate::state::AppState;

fn internal(e: impl std::fmt::Display) -> Response {
    warn!(error = %e, "auth route internal error");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

fn session_cookie_header(token: &str, ttl_secs: i64) -> (header::HeaderName, String) {
    // No `Secure` attribute: TLS termination is deployment-specific
    // (fronting proxy); HttpOnly + SameSite=Lax always.
    (
        header::SET_COOKIE,
        format!(
            "{}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={ttl_secs}",
            sessions::SESSION_COOKIE
        ),
    )
}

fn clear_cookie_header() -> (header::HeaderName, String) {
    (
        header::SET_COOKIE,
        format!(
            "{}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0",
            sessions::SESSION_COOKIE
        ),
    )
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct SessionInfo {
    pub user: String,
    pub role: Role,
}

/// POST /api/auth/login (password mode only — 404 elsewhere: the surface
/// does not exist under proxy/none).
#[utoipa::path(
    post,
    path = "/api/auth/login",
    tag = "auth",
    request_body = LoginRequest,
    responses(
        (status = 200, description = "Logged in; session cookie set", body = SessionInfo),
        (status = 401, description = "Bad credentials"),
        (status = 404, description = "Not in password auth mode"),
    ),
)]
pub async fn login(State(state): State<AppState>, Json(body): Json<LoginRequest>) -> Response {
    if !matches!(state.cfg.auth, AuthMode::Password) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let conn = state.db.lock().expect("db mutex poisoned");
    match users::verify(&conn, &body.username, &body.password) {
        Ok(Some(role)) => {
            let token = match sessions::create(&conn, &body.username, state.cfg.session_ttl_secs) {
                Ok(t) => t,
                Err(e) => return internal(e),
            };
            (
                StatusCode::OK,
                [session_cookie_header(&token, state.cfg.session_ttl_secs)],
                Json(SessionInfo {
                    user: body.username,
                    role,
                }),
            )
                .into_response()
        }
        Ok(None) => StatusCode::UNAUTHORIZED.into_response(),
        Err(e) => internal(e),
    }
}

/// POST /api/auth/logout — deletes the session, clears the cookie.
/// Idempotent: no session is still a 204.
#[utoipa::path(
    post,
    path = "/api/auth/logout",
    tag = "auth",
    responses((status = 204, description = "Session deleted, cookie cleared (idempotent)")),
)]
pub async fn logout(State(state): State<AppState>, req: Request) -> Response {
    if let Some(raw) = super::session_cookie(&req) {
        let conn = state.db.lock().expect("db mutex poisoned");
        if let Err(e) = sessions::delete(&conn, &raw) {
            return internal(e);
        }
    }
    (StatusCode::NO_CONTENT, [clear_cookie_header()]).into_response()
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct SetupRequest {
    pub setup_token: String,
    pub username: String,
    pub password: String,
}

/// POST /api/auth/setup — first-boot admin creation (D1): valid only while
/// zero users exist and only with the one-time token logged at startup.
/// Creates the admin and logs them in.
#[utoipa::path(
    post,
    path = "/api/auth/setup",
    tag = "auth",
    request_body = SetupRequest,
    responses(
        (status = 201, description = "Admin created and logged in", body = SessionInfo),
        (status = 401, description = "Wrong setup token"),
        (status = 404, description = "Not in password auth mode"),
        (status = 409, description = "Setup window closed (users already exist)"),
        (status = 422, description = "Empty username or password"),
    ),
)]
pub async fn setup(State(state): State<AppState>, Json(body): Json<SetupRequest>) -> Response {
    if !matches!(state.cfg.auth, AuthMode::Password) {
        return StatusCode::NOT_FOUND.into_response();
    }

    // Compare against the in-memory one-time token hash.
    {
        let guard = state.setup_token_hash.lock().expect("setup mutex poisoned");
        let Some(expected) = guard.as_ref() else {
            // No active setup window (users already exist).
            return StatusCode::CONFLICT.into_response();
        };
        if *expected != token_hash(&body.setup_token) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    if body.username.is_empty() || body.password.is_empty() {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }

    let conn = state.db.lock().expect("db mutex poisoned");
    // Re-check zero-users inside the write path; the PRIMARY KEY makes a
    // duplicate insert fail anyway (idempotence under retries).
    match users::count(&conn) {
        Ok(0) => {}
        Ok(_) => return StatusCode::CONFLICT.into_response(),
        Err(e) => return internal(e),
    }
    if let Err(e) = users::create(&conn, &body.username, &body.password, Role::Admin) {
        return internal(e);
    }
    // Burn the token: single-use.
    *state.setup_token_hash.lock().expect("setup mutex poisoned") = None;

    let token = match sessions::create(&conn, &body.username, state.cfg.session_ttl_secs) {
        Ok(t) => t,
        Err(e) => return internal(e),
    };
    (
        StatusCode::CREATED,
        [session_cookie_header(&token, state.cfg.session_ttl_secs)],
        Json(SessionInfo {
            user: body.username,
            role: Role::Admin,
        }),
    )
        .into_response()
}

/// `GET /api/auth/me` body. `kind` decides which identity fields are
/// present: `human` carries `user`+`role`, `device` carries
/// `deviceId`, `anonymous` carries neither.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WhoAmI {
    /// `human` | `device` | `anonymous`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// The role this identity ACTS with (mode-aware: REEVE_AUTH=none
    /// maps anonymous to admin); `null` = no access.
    pub effective_role: Option<Role>,
}

/// GET /api/auth/me — who am I, and what role am I acting with
/// (mode-aware: REEVE_AUTH=none reports anonymous acting as admin).
#[utoipa::path(
    get,
    path = "/api/auth/me",
    tag = "auth",
    responses((status = 200, description = "Current identity and effective role", body = WhoAmI)),
)]
pub async fn me(State(state): State<AppState>, identity: Identity) -> Response {
    let effective = state.effective_role(&identity);
    let body = match &identity {
        Identity::Human { user, role } => json!({
            "kind": "human", "user": user, "role": role,
            "effectiveRole": effective,
        }),
        Identity::Device { device_id } => json!({
            "kind": "device", "deviceId": device_id,
            "effectiveRole": effective,
        }),
        Identity::Anonymous => json!({
            "kind": "anonymous",
            "effectiveRole": effective,
        }),
    };
    Json(body).into_response()
}

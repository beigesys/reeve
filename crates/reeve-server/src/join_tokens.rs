//! Join tokens (docs/decisions/agent.md D4): operator-created, TTL +
//! max-uses (default 24h, 1 use), random, stored hashed.
//!
//! Placement: token MANAGEMENT (create/list/revoke) is a human operator
//! surface and lives here in reeve-server behind the D1 human auth +
//! role check (admin/operator). The device-facing route that CONSUMES a
//! join token (`POST /api/reeve/v1/enroll`) lives in crates/device-api.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use device_api::{Identity, Role, token_hash};
use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::warn;

use crate::db::now_secs;
use crate::state::AppState;

/// Prefix on every join token — recognizable in logs/scanners, distinct
/// from device (`rvd_`) and session (`rvh_`) tokens.
pub const JOIN_TOKEN_PREFIX: &str = "rvj_";
/// D4 default TTL: 24h.
pub const DEFAULT_TTL_SECS: i64 = 24 * 60 * 60;
/// D4 default max-uses: 1.
pub const DEFAULT_MAX_USES: i64 = 1;

/// Generate a fresh join token: `rvj_` + 64 lowercase hex chars
/// (256 bits from the OS CSPRNG).
pub fn generate_join_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("OS randomness unavailable");
    format!("{JOIN_TOKEN_PREFIX}{}", hex::encode(buf))
}

/// One join token row, as listed to operators (never the raw token).
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct JoinTokenInfo {
    pub token_hash: String,
    pub created_by: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub max_uses: i64,
    pub uses: i64,
    /// Set => re-enroll token bound to this existing device (D4).
    pub device_id: Option<String>,
    pub revoked_at: Option<i64>,
    /// Enrollment pre-assignment applied to the device that enrolls with
    /// this token (spec/reeve/11-fleet-model.md §11.3): hierarchy tiers
    /// and free-form tags.
    pub fleet: Option<String>,
    pub site: Option<String>,
    #[serde(rename = "type")]
    pub r#type: Option<String>,
    pub tags: Option<std::collections::BTreeMap<String, String>>,
}

/// Optional enrollment pre-assignment carried by a join token
/// (spec/reeve/11-fleet-model.md §11.3): the group a box lands in and
/// the tags it carries at first contact. All fields optional.
#[derive(Debug, Default, Clone)]
pub struct PreAssign {
    pub fleet: Option<String>,
    pub site: Option<String>,
    pub r#type: Option<String>,
    pub tags: Option<std::collections::BTreeMap<String, String>>,
}

/// Issue a join token with no pre-assignment (the D4 base case).
pub fn issue(
    conn: &Connection,
    created_by: &str,
    ttl_secs: i64,
    max_uses: i64,
    device_id: Option<&str>,
) -> rusqlite::Result<String> {
    issue_with(conn, created_by, ttl_secs, max_uses, device_id, &PreAssign::default())
}

/// Issue a join token: returns the raw token — the ONLY time it exists
/// server-side (only the hash is stored). `device_id` binds a re-enroll
/// token to an existing device (D4); `assign` pre-assigns the enrolling
/// device's group/tags (§11.3).
pub fn issue_with(
    conn: &Connection,
    created_by: &str,
    ttl_secs: i64,
    max_uses: i64,
    device_id: Option<&str>,
    assign: &PreAssign,
) -> rusqlite::Result<String> {
    let token = generate_join_token();
    let now = now_secs();
    let tags_json = assign
        .tags
        .as_ref()
        .map(|t| serde_json::to_string(t).expect("tags map serializes"));
    conn.execute(
        "INSERT INTO join_tokens
             (token_hash, created_by, created_at, expires_at, max_uses, device_id,
              fleet, site, \"type\", tags)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            token_hash(&token),
            created_by,
            now,
            now + ttl_secs,
            max_uses,
            device_id,
            assign.fleet,
            assign.site,
            assign.r#type,
            tags_json,
        ],
    )?;
    Ok(token)
}

/// List all join tokens, newest first.
pub fn list(conn: &Connection) -> rusqlite::Result<Vec<JoinTokenInfo>> {
    let mut stmt = conn.prepare(
        "SELECT token_hash, created_by, created_at, expires_at, max_uses, uses,
                device_id, revoked_at, fleet, site, \"type\", tags
         FROM join_tokens ORDER BY created_at DESC, token_hash",
    )?;
    let rows = stmt.query_map([], |row| {
        let tags_json: Option<String> = row.get(11)?;
        Ok(JoinTokenInfo {
            token_hash: row.get(0)?,
            created_by: row.get(1)?,
            created_at: row.get(2)?,
            expires_at: row.get(3)?,
            max_uses: row.get(4)?,
            uses: row.get(5)?,
            device_id: row.get(6)?,
            revoked_at: row.get(7)?,
            fleet: row.get(8)?,
            site: row.get(9)?,
            r#type: row.get(10)?,
            tags: tags_json.and_then(|t| serde_json::from_str(&t).ok()),
        })
    })?;
    rows.collect()
}

/// Revoke a join token by its hash. Idempotent.
pub fn revoke(conn: &Connection, hash: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE join_tokens SET revoked_at = ?1
         WHERE token_hash = ?2 AND revoked_at IS NULL",
        params![now_secs(), hash],
    )
}

/// Role gate for operator surfaces: 401 when nobody is authenticated
/// (mode-aware via `AppState::effective_role`), 403 when authenticated
/// below `needed`.
pub(crate) fn require_at_least(
    state: &AppState,
    identity: &Identity,
    needed: Role,
) -> Result<(), StatusCode> {
    match state.effective_role(identity) {
        Some(r) if r >= needed => Ok(()),
        Some(_) => Err(StatusCode::FORBIDDEN),
        None => Err(StatusCode::UNAUTHORIZED),
    }
}

fn internal(e: impl std::fmt::Display) -> Response {
    warn!(error = %e, "join token route internal error");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateJoinTokenRequest {
    /// Seconds until expiry; default 86400 (24h, D4).
    pub ttl_secs: Option<i64>,
    /// Default 1 (D4).
    pub max_uses: Option<i64>,
    /// Existing device to bind a re-enroll token to (D4).
    pub device_id: Option<String>,
    /// Optional enrollment pre-assignment (§11.3): the group the
    /// enrolling device lands in and the tags it carries at first
    /// contact.
    pub fleet: Option<String>,
    pub site: Option<String>,
    #[serde(rename = "type")]
    pub r#type: Option<String>,
    pub tags: Option<std::collections::BTreeMap<String, String>>,
}

/// `POST /api/join-tokens` body: the raw token, shown exactly once.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CreatedJoinToken {
    /// The raw join token (`rvj_<64 hex>`) — only the hash is stored.
    pub join_token: String,
    pub token_hash: String,
    pub expires_at: i64,
    pub max_uses: i64,
    pub device_id: Option<String>,
}

/// POST /api/join-tokens (admin/operator, D4). Returns the raw token —
/// shown exactly once.
#[utoipa::path(
    post,
    path = "/api/join-tokens",
    tag = "join-tokens",
    request_body = CreateJoinTokenRequest,
    responses(
        (status = 201, description = "Join token created; raw token shown exactly once", body = CreatedJoinToken),
        (status = 401, description = "Unauthenticated"),
        (status = 403, description = "Below operator role"),
        (status = 404, description = "Unknown device_id for a re-enroll token", body = device_api::ErrorBody),
        (status = 422, description = "Non-positive ttl_secs or max_uses", body = device_api::ErrorBody),
    ),
)]
pub async fn create(
    State(state): State<AppState>,
    identity: Identity,
    Json(body): Json<CreateJoinTokenRequest>,
) -> Response {
    if let Err(status) = require_at_least(&state, &identity, Role::Operator) {
        return status.into_response();
    }
    let ttl_secs = body.ttl_secs.unwrap_or(DEFAULT_TTL_SECS);
    let max_uses = body.max_uses.unwrap_or(DEFAULT_MAX_USES);
    if ttl_secs <= 0 || max_uses <= 0 {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": "ttl_secs and max_uses must be positive" })),
        )
            .into_response();
    }

    let created_by = match &identity {
        Identity::Human { user, .. } => user.clone(),
        // REEVE_AUTH=none: anonymous acts as admin (D1).
        _ => "anonymous".to_string(),
    };

    let conn = state.db.lock().expect("db mutex poisoned");
    if let Some(device_id) = &body.device_id {
        let exists: Option<i64> = match conn
            .query_row(
                "SELECT 1 FROM devices WHERE device_id = ?1",
                params![device_id],
                |row| row.get(0),
            )
            .optional()
        {
            Ok(v) => v,
            Err(e) => return internal(e),
        };
        if exists.is_none() {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "unknown device_id" })),
            )
                .into_response();
        }
    }
    let assign = PreAssign {
        fleet: body.fleet.clone(),
        site: body.site.clone(),
        r#type: body.r#type.clone(),
        tags: body.tags.clone(),
    };
    match issue_with(
        &conn,
        &created_by,
        ttl_secs,
        max_uses,
        body.device_id.as_deref(),
        &assign,
    ) {
        Ok(raw) => (
            StatusCode::CREATED,
            Json(json!({
                "join_token": raw,
                "token_hash": token_hash(&raw),
                "expires_at": now_secs() + ttl_secs,
                "max_uses": max_uses,
                "device_id": body.device_id,
            })),
        )
            .into_response(),
        Err(e) => internal(e),
    }
}

/// GET /api/join-tokens (admin/operator) — hashes and metadata only.
#[utoipa::path(
    get,
    path = "/api/join-tokens",
    tag = "join-tokens",
    responses(
        (status = 200, description = "All join tokens, newest first (hashes and metadata only)", body = Vec<JoinTokenInfo>),
        (status = 401, description = "Unauthenticated"),
        (status = 403, description = "Below operator role"),
    ),
)]
pub async fn index(State(state): State<AppState>, identity: Identity) -> Response {
    if let Err(status) = require_at_least(&state, &identity, Role::Operator) {
        return status.into_response();
    }
    let conn = state.db.lock().expect("db mutex poisoned");
    match list(&conn) {
        Ok(tokens) => Json(tokens).into_response(),
        Err(e) => internal(e),
    }
}

/// DELETE /api/join-tokens/{token_hash} (admin/operator) — revoke.
/// Idempotent: unknown or already-revoked is still 204.
#[utoipa::path(
    delete,
    path = "/api/join-tokens/{token_hash}",
    tag = "join-tokens",
    params(("token_hash" = String, Path, description = "Hex sha256 of the join token")),
    responses(
        (status = 204, description = "Revoked (idempotent: unknown or already-revoked is still 204)"),
        (status = 401, description = "Unauthenticated"),
        (status = 403, description = "Below operator role"),
    ),
)]
pub async fn delete(
    State(state): State<AppState>,
    identity: Identity,
    Path(hash): Path<String>,
) -> Response {
    if let Err(status) = require_at_least(&state, &identity, Role::Operator) {
        return status.into_response();
    }
    let conn = state.db.lock().expect("db mutex poisoned");
    match revoke(&conn, &hash) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "on").unwrap();
        crate::db::migrate(&mut conn).unwrap();
        conn
    }

    #[test]
    fn issue_list_revoke_round_trip() {
        let conn = test_conn();
        let raw = issue(&conn, "op", 3600, 1, None).unwrap();
        assert!(raw.starts_with(JOIN_TOKEN_PREFIX));
        assert_eq!(raw.len(), JOIN_TOKEN_PREFIX.len() + 64);

        let tokens = list(&conn).unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].token_hash, token_hash(&raw));
        assert_eq!(tokens[0].created_by, "op");
        assert_eq!(tokens[0].max_uses, 1);
        assert_eq!(tokens[0].uses, 0);
        assert_eq!(tokens[0].device_id, None);
        assert_eq!(tokens[0].revoked_at, None);

        revoke(&conn, &token_hash(&raw)).unwrap();
        assert!(list(&conn).unwrap()[0].revoked_at.is_some());
        // idempotent
        revoke(&conn, &token_hash(&raw)).unwrap();
    }

    #[test]
    fn reenroll_token_binds_device() {
        let conn = test_conn();
        conn.execute(
            "INSERT INTO devices (device_id, enrolled_at) VALUES ('dev-1', 0)",
            [],
        )
        .unwrap();
        let raw = issue(&conn, "op", 3600, 1, Some("dev-1")).unwrap();
        let tokens = list(&conn).unwrap();
        assert_eq!(tokens[0].device_id.as_deref(), Some("dev-1"));
        // binding to a nonexistent device violates the FK
        assert!(issue(&conn, "op", 3600, 1, Some("dev-nope")).is_err());
        let _ = raw;
    }
}

//! Human auth modes (docs/decisions/auth.md D1): password | proxy | none,
//! selected by REEVE_AUTH. All of it is tower middleware + extractors —
//! handlers receive `device_api::Identity` and never parse credentials.

pub mod cidr;
pub mod routes;
pub mod sessions;
pub mod users;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use device_api::{Identity, Role};
use std::net::SocketAddr;
use tracing::warn;

use crate::config::{AuthMode, ProxyConfig};
use crate::state::AppState;

/// What startup auth bootstrap decided; the caller logs it.
#[derive(Debug, Default)]
pub struct BootstrapReport {
    /// Loud startup notices (e.g. the REEVE_AUTH=none warning).
    pub notices: Vec<String>,
    /// One-time setup token (password mode, zero users). In-memory only;
    /// crash-only: a restart mints a fresh one.
    pub setup_token: Option<String>,
}

/// Startup auth work: purge expired sessions; in password mode with zero
/// users, mint the one-time setup token (D1 first boot). Idempotent.
pub fn bootstrap(state: &AppState) -> anyhow::Result<BootstrapReport> {
    let mut report = BootstrapReport::default();
    let conn = state.db.lock().expect("db mutex poisoned");

    let purged = sessions::purge_expired(&conn)?;
    if purged > 0 {
        report.notices.push(format!("purged {purged} expired sessions"));
    }

    match &state.cfg.auth {
        AuthMode::Password => {
            if users::count(&conn)? == 0 {
                let token = sessions::generate_session_token().replace("rvh_", "rvs_");
                *state.setup_token_hash.lock().expect("setup mutex poisoned") =
                    Some(device_api::token_hash(&token));
                report.setup_token = Some(token);
            }
        }
        AuthMode::Proxy(p) => {
            report.notices.push(format!(
                "REEVE_AUTH=proxy: trusting header {:?} from {} CIDR(s)",
                p.user_header,
                p.trusted.len()
            ));
        }
        AuthMode::None => {
            report.notices.push(
                "REEVE_AUTH=none: AUTH IS DISABLED — every request is anonymous admin. \
                 Bench and air-gapped dev only (docs/decisions/auth.md D1)."
                    .to_string(),
            );
        }
    }
    Ok(report)
}

/// Read the raw session token from the Cookie header, if any.
pub(crate) fn session_cookie(req: &Request) -> Option<String> {
    let cookies = req.headers().get(header::COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|kv| {
        let (k, v) = kv.trim().split_once('=')?;
        (k == sessions::SESSION_COOKIE).then(|| v.to_string())
    })
}

/// Mode-dispatching human auth middleware: resolves credentials into an
/// [`Identity`] request extension. Mount on every human-facing route
/// (device routes use `device_api::device_auth` instead).
///
/// It never rejects for *missing* credentials — that is the route guard's
/// job (`AppState::effective_role`) — but proxy mode rejects untrusted
/// peers outright (D1: never trust the header from the world).
pub async fn human_auth(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let identity = match &state.cfg.auth {
        AuthMode::Password => {
            let ttl = state.cfg.session_ttl_secs;
            match session_cookie(&req) {
                Some(raw) => {
                    let conn = state.db.lock().expect("db mutex poisoned");
                    match sessions::validate_and_slide(&conn, &raw, ttl) {
                        Ok(Some((user, role))) => Identity::Human { user, role },
                        Ok(None) => Identity::Anonymous,
                        Err(e) => {
                            warn!(error = %e, "session lookup failed");
                            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                        }
                    }
                }
                None => Identity::Anonymous,
            }
        }
        AuthMode::Proxy(p) => match proxy_identity(p, &req) {
            Ok(identity) => identity,
            Err(status) => return status.into_response(),
        },
        AuthMode::None => Identity::Anonymous,
    };

    req.extensions_mut().insert(identity);
    next.run(req).await
}

/// Proxy mode (D1): the peer must be inside REEVE_PROXY_TRUSTED_CIDR or
/// the request is rejected — including when the peer address is unknown.
fn proxy_identity(p: &ProxyConfig, req: &Request) -> Result<Identity, StatusCode> {
    let Some(ConnectInfo(peer)) = req.extensions().get::<ConnectInfo<SocketAddr>>() else {
        warn!("proxy mode: no peer address on request; refusing");
        return Err(StatusCode::UNAUTHORIZED);
    };
    if !p.trusted.iter().any(|c| c.contains(peer.ip())) {
        warn!(peer = %peer.ip(), "proxy mode: peer outside trusted CIDRs; refusing");
        return Err(StatusCode::UNAUTHORIZED);
    }

    let header_val = |name: &str| {
        req.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };

    let Some(user) = header_val(&p.user_header) else {
        return Ok(Identity::Anonymous);
    };
    // Role header absent => admin (the proxy already gates access);
    // unknown value => viewer (least privilege on misconfiguration).
    let role = match p.role_header.as_deref().and_then(header_val) {
        None => Role::Admin,
        Some(r) => r.parse::<Role>().unwrap_or_else(|_| {
            warn!(role = %r, "proxy role header unparseable; demoting to viewer");
            Role::Viewer
        }),
    };
    Ok(Identity::Human { user, role })
}

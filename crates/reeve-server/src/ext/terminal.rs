//! ext-terminal (REV-002) — the reeve-server terminal bridge (build
//! item C8).
//!
//! Normative source: spec/reeve/03-terminal.md §5 — guardrails first,
//! byte plumbing deliberately trivial:
//! - §5.1 transport: agent leg = a server-opened (EVEN id) rev-001
//!   sub-channel, purpose `rev-002/terminal`, `open.meta` =
//!   [`TerminalOpenMeta`] (sessionId, PTY size, TERM — nothing else).
//!   UI leg = the one genuinely bidirectional UI websocket. No
//!   channel, no terminal: initiation fails immediately with "device
//!   offline"; nothing queues.
//! - §5.2 enablement: desired state ONLY. Before initiating, the
//!   server parses `config/terminal.yaml` out of ITS OWN current
//!   render of the device ([`rendered_terminal_config`]) — defense in
//!   depth, both sides check; the agent independently refuses when
//!   its converged bundle disables the terminal. There is NO runtime
//!   toggle anywhere in this module.
//! - §5.3 lifecycle: operator-initiated, short-lived (idle timeout +
//!   hard cap from the enablement config), any leg failure closes the
//!   whole session; reconnection is a new session, new id, new audit
//!   row.
//! - §5.4 audit: a `terminal_sessions` row is written at initiation
//!   BEFORE any bytes flow and finalized at close; denied initiations
//!   are recorded too; crash recovery finalizes dangling rows as
//!   `server-restart` (lib.rs bootstrap). Lifecycle transitions are
//!   published as `terminal-session` events — metadata only.
//! - §5.5 bridge conduct: bytes only. This module never parses,
//!   transforms, or logs session content — it counts bytes
//!   (accounting, not interpretation). No recording exists in
//!   rev-002/1 ("no recording/replay in rev-002/1" is MUST-level;
//!   the C8 build-charter mention of optional recording is
//!   deliberately NOT implemented — recorded decision).
//! - Authorization: distinct auditable privilege (§5.6) — operator+
//!   AND a password/proxy auth mode (docs/decisions/auth.md D1:
//!   "Terminal (REV-002) enables only under password/proxy modes");
//!   under REEVE_AUTH=none there is no attributable username, so
//!   initiation is refused outright.

use std::io::Read as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse as _, Response};
use device_api::{Identity, Role};
use futures_util::{SinkExt as _, StreamExt as _};
use reeve_types::reeve::channel::{MAX_FRAME_BYTES, PURPOSE_TERMINAL};
use reeve_types::reeve::events::{SseEvent, TerminalPhase, TerminalSessionEvent};
use reeve_types::reeve::terminal::{TERMINAL_CONFIG_PATH, TerminalConfig, TerminalOpenMeta};
use rusqlite::{OptionalExtension as _, params};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::channels::{Outbound, SubChannel, SubEvent};
use crate::config::AuthMode;
use crate::db::now_secs;
use crate::events::EventHub;
use crate::state::AppState;

/// How long the agent gets to answer the sub-channel `open` before
/// the initiation is declared failed (the channel was up when we
/// checked; an unanswered open means it died under us — §5.1).
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);

/// Requested PTY geometry, from the operator websocket's query string
/// (§5.1: `open.meta` carries sessionId, requested PTY size, TERM).
#[derive(Debug, Deserialize)]
pub struct TerminalQuery {
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    pub term: Option<String>,
}

/// Server-assigned session id (§5.3): `ts-<128 random bits, hex>`.
fn new_session_id() -> String {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).expect("OS randomness unavailable");
    format!("ts-{}", hex::encode(buf))
}

fn emit_phase(events: &EventHub, session_id: &str, device_id: &str, user: &str, phase: TerminalPhase) {
    events.emit(SseEvent::TerminalSession(TerminalSessionEvent {
        ts: EventHub::now_ts(),
        session_id: session_id.to_string(),
        device_id: device_id.to_string(),
        phase,
        user: user.to_string(),
    }));
}

/// Audit a DENIED initiation (§5.4: "denied initiations MUST be
/// recorded too"): one closed row, started == ended, reason = denial.
fn audit_denied(
    state: &AppState,
    session_id: &str,
    device_id: &str,
    user: &str,
    reason: &str,
    enablement_revision: Option<i64>,
) {
    let now = now_secs();
    let conn = state.db.lock().expect("db mutex poisoned");
    if let Err(e) = conn.execute(
        "INSERT INTO terminal_sessions
             (session_id, device_id, username, started_at, ended_at,
              close_reason, enablement_revision)
         VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6)",
        params![session_id, device_id, user, now, reason, enablement_revision],
    ) {
        warn!(error = %e, "terminal audit (denied) write failed");
    }
    drop(conn);
    emit_phase(&state.events, session_id, device_id, user, TerminalPhase::Denied);
}

/// Finalize a live session's audit row (§5.4). Always writes
/// `ended_at`; the crash path is covered by startup recovery.
fn audit_finalize(
    state: &AppState,
    session_id: &str,
    close_reason: &str,
    bytes_up: u64,
    bytes_down: u64,
) {
    let conn = state.db.lock().expect("db mutex poisoned");
    if let Err(e) = conn.execute(
        "UPDATE terminal_sessions
         SET ended_at = ?2, close_reason = ?3, bytes_up = ?4, bytes_down = ?5
         WHERE session_id = ?1 AND ended_at IS NULL",
        params![session_id, now_secs(), close_reason, bytes_up as i64, bytes_down as i64],
    ) {
        warn!(error = %e, "terminal audit finalize failed");
    }
}

/// The server-side enablement check (§5.2 defense in depth): parse
/// [`TERMINAL_CONFIG_PATH`] out of the device's CURRENT render bundle.
/// Absent bundle, absent file, or an unparseable file all evaluate to
/// the disabled default (default-deny). Returns the enablement
/// revision alongside (§5.4: "the enablement commit id in effect").
pub fn rendered_terminal_config(
    state: &AppState,
    device_id: &str,
) -> anyhow::Result<(TerminalConfig, Option<i64>)> {
    // Freshness: render on demand if this device's row is behind the
    // local head, so a just-committed enablement (or disablement!) is
    // honored. Degrade to the stored render on error — same posture
    // as the manifest route.
    if let Err(e) = crate::render::ensure_current(state, device_id) {
        debug!(device = %device_id, error = %e, "ensure_current failed; checking stored render");
    }

    let conn = state.db.lock().expect("db mutex poisoned");
    let row: Option<(Option<String>, i64)> = conn
        .query_row(
            "SELECT layer_digest, rendered_revision FROM device_manifests
             WHERE device_id = ?1",
            params![device_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((layer_digest, revision)) = row else {
        return Ok((TerminalConfig::default(), None)); // never rendered
    };
    let Some(layer_digest) = layer_digest else {
        return Ok((TerminalConfig::default(), Some(revision))); // zero-app bundle
    };
    let blob: Option<Vec<u8>> = conn
        .query_row(
            "SELECT content FROM bundle_blobs WHERE digest = ?1",
            params![layer_digest],
            |r| r.get(0),
        )
        .optional()?;
    drop(conn);
    let Some(tarball) = blob else {
        return Ok((TerminalConfig::default(), Some(revision)));
    };

    // Scan the tar.gz for config/terminal.yaml. Anything short of a
    // well-formed enablement file is disabled (§5.2 default-deny).
    let gz = flate2::read::GzDecoder::new(tarball.as_slice());
    let mut archive = tar::Archive::new(gz);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let is_config = entry
            .path()
            .map(|p| p.to_string_lossy() == TERMINAL_CONFIG_PATH)
            .unwrap_or(false);
        if !is_config {
            continue;
        }
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        let cfg = serde_yaml_ng::from_slice::<TerminalConfig>(&bytes).unwrap_or_default();
        return Ok((cfg, Some(revision)));
    }
    Ok((TerminalConfig::default(), Some(revision)))
}

/// GET /api/reeve/v1/terminal/{device_id} — the UI leg (§5.1),
/// operator-initiated (§5.3). Every denial is audited (§5.4) and
/// answered BEFORE upgrade.
#[utoipa::path(
    get,
    path = "/api/reeve/v1/terminal/{device_id}",
    tag = "terminal",
    params(
        ("device_id" = String, Path, description = "Target device id"),
        ("cols" = Option<u16>, Query, description = "Requested PTY columns"),
        ("rows" = Option<u16>, Query, description = "Requested PTY rows"),
        ("term" = Option<String>, Query, description = "Requested TERM value"),
    ),
    responses(
        (status = 101, description = "WebSocket upgrade: byte-transparent terminal bridge (xterm.js peer; spec/reeve/03-terminal.md §5)"),
        (status = 401, description = "Unauthenticated"),
        (status = 403, description = "Below operator role, terminal disabled by desired state, or unattributable auth mode"),
        (status = 404, description = "Unknown device"),
        (status = 409, description = "Device offline or session limit reached"),
    ),
)]
pub async fn terminal_route(
    State(state): State<AppState>,
    identity: Identity,
    Path(device_id): Path<String>,
    Query(query): Query<TerminalQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    // Who to attribute a denial to (§5.4: denied initiations —
    // authorization failures included — MUST be recorded too).
    let audit_user = match &identity {
        Identity::Human { user, .. } => user.clone(),
        Identity::Device { device_id } => format!("device:{device_id}"),
        Identity::Anonymous => "anonymous".to_string(),
    };
    // D1: terminal only under password/proxy modes — REEVE_AUTH=none
    // has no attributable username, so refuse outright, even though
    // none-mode anonymous acts as admin elsewhere.
    if matches!(state.cfg.auth, AuthMode::None) {
        let session_id = new_session_id();
        audit_denied(
            &state,
            &session_id,
            &device_id,
            &audit_user,
            "denied: REEVE_AUTH=none (docs/decisions/auth.md D1)",
            None,
        );
        return (
            StatusCode::FORBIDDEN,
            axum::Json(json!({
                "error": "terminal is disabled under REEVE_AUTH=none (docs/decisions/auth.md D1)"
            })),
        )
            .into_response();
    }
    // §5.6: initiation is a distinct, auditable privilege — operator+.
    if let Err(status) = crate::join_tokens::require_at_least(&state, &identity, Role::Operator) {
        let session_id = new_session_id();
        audit_denied(
            &state,
            &session_id,
            &device_id,
            &audit_user,
            "denied: authorization failure (operator+ required, §5.6)",
            None,
        );
        return status.into_response();
    }
    let Identity::Human { user, .. } = &identity else {
        // Devices carry no human role; require_at_least already
        // refused them — this is belt and braces.
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let user = user.clone();

    // Unknown device: 404, still audited (denials are recorded, §5.4).
    let known: bool = {
        let conn = state.db.lock().expect("db mutex poisoned");
        match conn
            .query_row(
                "SELECT 1 FROM devices WHERE device_id = ?1",
                params![device_id],
                |_| Ok(()),
            )
            .optional()
        {
            Ok(row) => row.is_some(),
            Err(e) => {
                warn!(error = %e, "terminal device lookup failed");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    };
    let session_id = new_session_id();
    if !known {
        audit_denied(&state, &session_id, &device_id, &user, "denied: unknown device", None);
        return StatusCode::NOT_FOUND.into_response();
    }

    // §5.2 defense in depth: the server refuses to initiate when its
    // OWN render of the device does not enable the terminal.
    let (config, enablement_revision) = match rendered_terminal_config(&state, &device_id) {
        Ok(pair) => pair,
        Err(e) => {
            warn!(device = %device_id, error = %e, "terminal enablement check failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if !config.enabled {
        audit_denied(
            &state,
            &session_id,
            &device_id,
            &user,
            "denied: terminal not enabled in rendered desired state (§5.2)",
            enablement_revision,
        );
        return (
            StatusCode::FORBIDDEN,
            axum::Json(json!({
                "error": "terminal not enabled in this device's desired state \
                          (spec/reeve/03-terminal.md §5.2 — enablement is a tree revision)"
            })),
        )
            .into_response();
    }

    // §5.1: no channel, no terminal — fail immediately, never queue.
    let Some(agent) = state.channels.sender(&device_id) else {
        audit_denied(
            &state,
            &session_id,
            &device_id,
            &user,
            "denied: device offline (no channel)",
            enablement_revision,
        );
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({ "error": "device offline" })),
        )
            .into_response();
    };

    // §5.4: the audit record is written at initiation, BEFORE any
    // bytes flow.
    {
        let conn = state.db.lock().expect("db mutex poisoned");
        if let Err(e) = conn.execute(
            "INSERT INTO terminal_sessions
                 (session_id, device_id, username, started_at, enablement_revision)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, device_id, user, now_secs(), enablement_revision],
        ) {
            warn!(error = %e, "terminal audit (initiation) write failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    emit_phase(&state.events, &session_id, &device_id, &user, TerminalPhase::Requested);
    info!(session = %session_id, device = %device_id, user = %user, "terminal session requested");

    let meta = TerminalOpenMeta {
        session_id: session_id.clone(),
        cols: query.cols.unwrap_or(80),
        rows: query.rows.unwrap_or(24),
        term: query.term,
    };
    ws.max_message_size(MAX_FRAME_BYTES)
        .max_frame_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| {
            bridge(state, agent, socket, session_id, device_id, user, meta, config)
        })
        .into_response()
}

/// The byte bridge (§5.5): open the agent leg, then relay opaque
/// payloads both ways, counting bytes and enforcing §5.3 limits.
/// Never parses content in either direction.
#[allow(clippy::too_many_arguments)]
async fn bridge(
    state: AppState,
    agent: mpsc::Sender<Outbound>,
    ui: WebSocket,
    session_id: String,
    device_id: String,
    user: String,
    meta: TerminalOpenMeta,
    config: TerminalConfig,
) {
    // Agent leg: open the rev-002/terminal sub-channel (§5.1).
    let (reply_tx, reply_rx) = oneshot::channel();
    let meta_json = serde_json::to_value(&meta).expect("open meta serializes");
    let sent = agent
        .send(Outbound::OpenSub {
            purpose: PURPOSE_TERMINAL.to_string(),
            meta: Some(meta_json),
            reply: reply_tx,
        })
        .await
        .is_ok();
    let opened: Result<SubChannel, String> = if !sent {
        Err("channel lost before open".into())
    } else {
        match tokio::time::timeout(OPEN_TIMEOUT, reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("channel closed during open".into()),
            Err(_) => Err("agent did not answer open in time".into()),
        }
    };

    let sub = match opened {
        Ok(sub) => sub,
        Err(reason) => {
            // Agent-side refusal (its own §5.2 check — e.g. "not
            // enabled") or a dying channel: deny, audit, drop the UI
            // leg. The reason is metadata, not session content.
            audit_finalize(
                &state,
                &session_id,
                &format!("denied: {reason}"),
                0,
                0,
            );
            emit_phase(&state.events, &session_id, &device_id, &user, TerminalPhase::Denied);
            info!(session = %session_id, device = %device_id, %reason, "terminal open refused");
            let mut ui = ui;
            let _ = ui
                .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                    code: 1011,
                    reason: reason.into(),
                })))
                .await;
            return;
        }
    };

    // Opened: complete the audit row and announce (§5.4).
    let sub_id = sub.id;
    {
        let conn = state.db.lock().expect("db mutex poisoned");
        if let Err(e) = conn.execute(
            "UPDATE terminal_sessions SET opened_at = ?2 WHERE session_id = ?1",
            params![session_id, now_secs()],
        ) {
            warn!(error = %e, "terminal audit (opened) write failed");
        }
    }
    emit_phase(&state.events, &session_id, &device_id, &user, TerminalPhase::Opened);
    info!(session = %session_id, device = %device_id, sub_channel = sub_id, "terminal session opened");

    // Relay (§5.5, bytes only). Two independent pump tasks so a stall
    // in one direction never deadlocks the other; the coordinator
    // enforces the §5.3 limits and finalizes.
    let bytes_up = Arc::new(AtomicU64::new(0)); // UI -> agent
    let bytes_down = Arc::new(AtomicU64::new(0)); // agent -> UI
    let last_activity = Arc::new(std::sync::Mutex::new(Instant::now()));

    let (mut ui_tx, mut ui_rx) = ui.split();
    let mut sub_rx = sub.rx;

    let up_agent = agent.clone();
    let up_count = bytes_up.clone();
    let up_activity = last_activity.clone();
    let up = tokio::spawn(async move {
        loop {
            match ui_rx.next().await {
                Some(Ok(Message::Binary(payload))) => {
                    up_count.fetch_add(payload.len() as u64, Ordering::Relaxed);
                    *up_activity.lock().expect("activity mutex") = Instant::now();
                    if up_agent
                        .send(Outbound::Data { id: sub_id, payload: payload.to_vec() })
                        .await
                        .is_err()
                    {
                        return "channel lost";
                    }
                }
                // The in-band encoding is agent-owned; the bridge
                // relays text frames as their raw bytes without
                // looking inside (§5.5).
                Some(Ok(Message::Text(text))) => {
                    let payload = text.as_bytes().to_vec();
                    up_count.fetch_add(payload.len() as u64, Ordering::Relaxed);
                    *up_activity.lock().expect("activity mutex") = Instant::now();
                    if up_agent
                        .send(Outbound::Data { id: sub_id, payload })
                        .await
                        .is_err()
                    {
                        return "channel lost";
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_))) | None => return "operator closed",
                Some(Err(_)) => return "operator socket error",
            }
        }
    });

    let down_count = bytes_down.clone();
    let down_activity = last_activity.clone();
    let down = tokio::spawn(async move {
        loop {
            match sub_rx.recv().await {
                Some(SubEvent::Data(payload)) => {
                    down_count.fetch_add(payload.len() as u64, Ordering::Relaxed);
                    *down_activity.lock().expect("activity mutex") = Instant::now();
                    if ui_tx.send(Message::Binary(payload.into())).await.is_err() {
                        return "operator closed";
                    }
                }
                Some(SubEvent::Closed) | None => return "agent closed",
            }
        }
    });

    // §5.3: both sides enforce limits — these are the server's.
    let idle_timeout = Duration::from_secs(config.idle_timeout_secs.max(1));
    let hard_cap = Duration::from_secs(config.hard_cap_secs.max(1));
    let idle_watch = {
        let last_activity = last_activity.clone();
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let idle_for = last_activity.lock().expect("activity mutex").elapsed();
                if idle_for >= idle_timeout {
                    return;
                }
            }
        }
    };

    let mut up = up;
    let mut down = down;
    let close_reason: &str = tokio::select! {
        r = &mut up => r.unwrap_or("bridge task failed"),
        r = &mut down => r.unwrap_or("bridge task failed"),
        _ = tokio::time::sleep(hard_cap) => "hard cap (§5.3)",
        _ = idle_watch => "idle timeout (§5.3)",
    };
    up.abort();
    down.abort();

    // Close the agent leg (best-effort; the channel may already be
    // gone — sub-channel loss is a normal event, §4.2).
    let _ = agent.try_send(Outbound::CloseSub {
        id: sub_id,
        reason: Some(close_reason.to_string()),
    });

    audit_finalize(
        &state,
        &session_id,
        close_reason,
        bytes_up.load(Ordering::Relaxed),
        bytes_down.load(Ordering::Relaxed),
    );
    emit_phase(&state.events, &session_id, &device_id, &user, TerminalPhase::Closed);
    info!(
        session = %session_id,
        device = %device_id,
        reason = close_reason,
        bytes_up = bytes_up.load(Ordering::Relaxed),
        bytes_down = bytes_down.load(Ordering::Relaxed),
        "terminal session closed"
    );
}

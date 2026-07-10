//! C8 ext-terminal end-to-end: the operator-websocket ⇄ sub-channel
//! byte bridge with a fake agent, plus every refusal guardrail —
//! against the real router on a real listener.
//!
//! Spec sources: spec/reeve/03-terminal.md §5.1 (transport, no
//! channel = no terminal), §5.2 (enablement from desired state only,
//! both sides check), §5.4 (audit rows incl. denials + username +
//! byte counts; server-restart finalization), §5.5 (relay bytes
//! only); docs/decisions/auth.md D1 (password/proxy modes only).
#![cfg(feature = "ext-terminal")]

mod common;

use common::*;
use futures_util::{SinkExt as _, StreamExt as _};
use reeve_server::config::AuthMode;
use reeve_server::state::AppState;
use reeve_server::{auth, render};
use reeve_types::reeve::channel::{
    ControlFrame, PURPOSE_TERMINAL, decode_data_frame, encode_data_frame,
};
use reeve_types::reeve::events::{SseEvent, TerminalPhase};
use reeve_types::reeve::terminal::{TERMINAL_CONFIG_PATH, TerminalOpenMeta};
use revision_store::digest_of;
use rusqlite::params;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::header;
use tokio_tungstenite::tungstenite::protocol::Message;

// ------------------------------------------------------------- harness

/// Operator user + session cookie (password mode: the D1 mode where
/// the terminal is allowed and the username is attributable).
fn operator_cookie(state: &AppState, user: &str, role: device_api::Role) -> String {
    let conn = state.db.lock().unwrap();
    auth::users::create(&conn, user, "hunter2hunter2", role).unwrap();
    let token = auth::sessions::create(&conn, user, 3600).unwrap();
    format!("{}={token}", auth::sessions::SESSION_COOKIE)
}

/// Install a crafted render bundle for a device — the §5.2 enablement
/// fixture: the server's own render of the device is what its
/// defense-in-depth check parses. `rendered_revision` 0 == the empty
/// tree head, so ensure_current keeps this bundle current.
fn install_bundle(state: &AppState, device_id: &str, files: &[(&str, &str)]) {
    let fileset: desired_state::FileSet = files
        .iter()
        .map(|(p, c)| (p.to_string(), c.as_bytes().to_vec()))
        .collect();
    let tarball = render::pack_bundle(&fileset).unwrap();
    let layer_digest = digest_of(&tarball);
    let conn = state.db.lock().unwrap();
    let now = reeve_server::db::now_secs();
    conn.execute(
        "INSERT INTO bundle_blobs (digest, content, created_at) VALUES (?1, ?2, ?3)",
        params![layer_digest, tarball, now],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO device_manifests
             (device_id, manifest_version, counter, generation, content_digest,
              bundle_digest, layer_digest, manifest_json, etag,
              rendered_revision, updated_at)
         VALUES (?1, 1, 1, 1, 'sha256:test', 'sha256:test-bundle', ?2, '{}', 'e', 0, ?3)",
        params![device_id, layer_digest, now],
    )
    .unwrap();
}

const ENABLED_TERMINAL_YAML: &str = "enabled: true\nshell: /bin/sh\n";

fn terminal_request(
    addr: std::net::SocketAddr,
    device: &str,
    cookie: Option<&str>,
) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let mut request =
        format!("ws://{addr}/api/reeve/v1/terminal/{device}?cols=100&rows=30&term=xterm")
            .into_client_request()
            .unwrap();
    if let Some(cookie) = cookie {
        request
            .headers_mut()
            .insert(header::COOKIE, cookie.parse().unwrap());
    }
    request
}

/// Assert a websocket connect is refused pre-upgrade with `status`.
async fn assert_refused(
    request: tokio_tungstenite::tungstenite::handshake::client::Request,
    status: u16,
    what: &str,
) {
    match tokio_tungstenite::connect_async(request).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status(), status, "{what}");
        }
        Ok(_) => panic!("{what}: session must be refused"),
        Err(other) => panic!("{what}: expected HTTP {status}, got {other:?}"),
    }
}

/// The audit row (§5.4).
#[derive(Debug)]
struct AuditRow {
    username: String,
    opened_at: Option<i64>,
    ended_at: Option<i64>,
    close_reason: Option<String>,
    bytes_up: i64,
    bytes_down: i64,
    enablement_revision: Option<i64>,
}

fn audit_rows(state: &AppState, device: &str) -> Vec<AuditRow> {
    let conn = state.db.lock().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT username, opened_at, ended_at, close_reason, bytes_up, bytes_down,
                    enablement_revision
             FROM terminal_sessions WHERE device_id = ?1 ORDER BY started_at",
        )
        .unwrap();
    let rows = stmt
        .query_map(params![device], |r| {
            Ok(AuditRow {
                username: r.get(0)?,
                opened_at: r.get(1)?,
                ended_at: r.get(2)?,
                close_reason: r.get(3)?,
                bytes_up: r.get(4)?,
                bytes_down: r.get(5)?,
                enablement_revision: r.get(6)?,
            })
        })
        .unwrap();
    rows.map(Result::unwrap).collect()
}

// --------------------------------------------------------------- tests

/// The full happy path: operator websocket -> even sub-channel with
/// TerminalOpenMeta -> byte relay BOTH ways (opaque, counted) ->
/// close -> finalized audit row with username and byte counts, and
/// the requested/opened/closed event trail (§5.1/§5.4/§5.5).
#[tokio::test]
async fn bridge_relays_bytes_both_ways_and_audits() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path(), AuthMode::Password);
    let cookie = operator_cookie(&state, "op", device_api::Role::Operator);
    let token = add_device(&state, "dev-1");
    install_bundle(
        &state,
        "dev-1",
        &[
            (TERMINAL_CONFIG_PATH, ENABLED_TERMINAL_YAML),
            ("apps/web/compose.yml", "services: {}\n"),
        ],
    );
    let addr = serve(app).await;
    let mut events = state.events.subscribe(None).rx;

    let mut agent = connect_agent(addr, &token).await;

    // Operator leg connects; the server opens the sub-channel.
    let (mut ui, _resp) =
        tokio_tungstenite::connect_async(terminal_request(addr, "dev-1", Some(&cookie)))
            .await
            .expect("operator websocket");

    // Agent sees `open`: EVEN id, terminal purpose, bootstrap meta
    // ONLY (sessionId, PTY size, TERM — §5.1).
    let open = recv_control(&mut agent).await;
    let ControlFrame::Open { id, purpose, meta } = open else {
        panic!("expected open, got {open:?}");
    };
    assert_eq!(id % 2, 0, "server-opened sub-channels use even ids (§4.2)");
    assert_eq!(purpose, PURPOSE_TERMINAL);
    let meta: TerminalOpenMeta = serde_json::from_value(meta.expect("open.meta")).unwrap();
    assert!(meta.session_id.starts_with("ts-"), "{}", meta.session_id);
    assert_eq!((meta.cols, meta.rows), (100, 30));
    assert_eq!(meta.term.as_deref(), Some("xterm"));
    send_control(&mut agent, &ControlFrame::Accept { id }).await;

    // UI -> agent: opaque bytes, relayed verbatim (§5.5 — the 0x00
    // prefix is the agent-owned in-band encoding; the bridge does not
    // parse it).
    ui.send(Message::Binary(b"\x00keys".to_vec().into()))
        .await
        .unwrap();
    let frame = recv_binary(&mut agent).await;
    assert_eq!(decode_data_frame(&frame), Some((id, &b"\x00keys"[..])));

    // agent -> UI: PTY output bytes, relayed verbatim.
    send_binary(&mut agent, encode_data_frame(id, b"\x00output")).await;
    let msg = tokio::time::timeout(std::time::Duration::from_secs(10), ui.next())
        .await
        .expect("relayed output")
        .expect("ui socket open")
        .expect("frame");
    assert_eq!(msg, Message::Binary(b"\x00output".to_vec().into()));

    // Operator closes: the session ends, the agent leg is closed, the
    // audit row is finalized with counts and the username.
    ui.close(None).await.unwrap();
    wait_for(
        || {
            audit_rows(&state, "dev-1")
                .first()
                .is_some_and(|r| r.ended_at.is_some())
        },
        "audit row finalized",
    )
    .await;

    let rows = audit_rows(&state, "dev-1");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.username, "op");
    assert!(row.opened_at.is_some());
    assert_eq!(row.bytes_up, 5, "\\x00keys");
    assert_eq!(row.bytes_down, 7, "\\x00output");
    assert!(
        row.close_reason.as_deref().unwrap().contains("operator"),
        "{:?}",
        row.close_reason
    );
    assert_eq!(row.enablement_revision, Some(0), "§5.4 enablement commit id");

    // The agent leg got `close` for the sub-channel.
    let closed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match recv_control(&mut agent).await {
                ControlFrame::Close { id: cid, .. } => break cid,
                ControlFrame::Ping { nonce } => {
                    send_control(&mut agent, &ControlFrame::Pong { nonce }).await;
                }
                _ => continue,
            }
        }
    })
    .await
    .expect("sub-channel close");
    assert_eq!(closed, id);

    // Event trail (§5.4 metadata only): requested -> opened -> closed.
    let mut phases = Vec::new();
    while phases.len() < 3 {
        let ev = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
            .await
            .expect("terminal-session events")
            .unwrap();
        if let SseEvent::TerminalSession(t) = ev.event {
            assert_eq!(t.user, "op");
            assert_eq!(t.session_id, meta.session_id);
            phases.push(t.phase);
        }
    }
    assert_eq!(
        phases,
        [TerminalPhase::Requested, TerminalPhase::Opened, TerminalPhase::Closed]
    );
}

/// §5.2 defense in depth: the SERVER refuses to initiate when its own
/// render does not enable the terminal — audited as denied.
#[tokio::test]
async fn disabled_render_refuses_session() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path(), AuthMode::Password);
    let cookie = operator_cookie(&state, "op", device_api::Role::Operator);
    let token = add_device(&state, "dev-1");
    // Rendered bundle WITHOUT config/terminal.yaml: absent file =
    // disabled (default-deny, §5.2).
    install_bundle(&state, "dev-1", &[("apps/web/compose.yml", "services: {}\n")]);
    let addr = serve(app).await;
    let _agent = connect_agent(addr, &token).await; // online, but not enabled

    assert_refused(
        terminal_request(addr, "dev-1", Some(&cookie)),
        403,
        "disabled desired state must refuse initiation",
    )
    .await;

    let rows = audit_rows(&state, "dev-1");
    assert_eq!(rows.len(), 1, "denied initiations MUST be recorded (§5.4)");
    assert!(rows[0].ended_at.is_some());
    assert!(
        rows[0].close_reason.as_deref().unwrap().contains("not enabled"),
        "{:?}",
        rows[0].close_reason
    );
}

/// D1: terminal only under password/proxy modes — REEVE_AUTH=none is
/// refused even though anonymous acts as admin elsewhere.
#[tokio::test]
async fn none_auth_mode_refuses_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path(), AuthMode::None);
    let token = add_device(&state, "dev-1");
    install_bundle(&state, "dev-1", &[(TERMINAL_CONFIG_PATH, ENABLED_TERMINAL_YAML)]);
    let addr = serve(app).await;
    let _agent = connect_agent(addr, &token).await;

    assert_refused(
        terminal_request(addr, "dev-1", None),
        403,
        "REEVE_AUTH=none must refuse terminal sessions (D1)",
    )
    .await;
    // §5.4: authorization denials are recorded too.
    let rows = audit_rows(&state, "dev-1");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].username, "anonymous");
    assert!(
        rows[0].close_reason.as_deref().unwrap().contains("REEVE_AUTH=none"),
        "{:?}",
        rows[0].close_reason
    );
}

/// §5.6: initiation is a distinct privilege — viewer is refused.
#[tokio::test]
async fn viewer_role_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path(), AuthMode::Password);
    let cookie = operator_cookie(&state, "watcher", device_api::Role::Viewer);
    let _token = add_device(&state, "dev-1");
    let addr = serve(app).await;

    assert_refused(
        terminal_request(addr, "dev-1", Some(&cookie)),
        403,
        "viewer must not initiate terminal sessions",
    )
    .await;
    // No cookie at all: 401.
    assert_refused(
        terminal_request(addr, "dev-1", None),
        401,
        "anonymous must not initiate terminal sessions",
    )
    .await;
    // §5.4: both authorization failures are recorded, attributed.
    // (started_at has second granularity — compare as a set.)
    let rows = audit_rows(&state, "dev-1");
    assert_eq!(rows.len(), 2);
    let mut users: Vec<&str> = rows.iter().map(|r| r.username.as_str()).collect();
    users.sort_unstable();
    assert_eq!(users, ["anonymous", "watcher"]);
    for row in &rows {
        assert!(
            row.close_reason.as_deref().unwrap().contains("authorization"),
            "{:?}",
            row.close_reason
        );
    }
}

/// §5.1: no channel, no terminal — initiation fails immediately with
/// "device offline"; nothing queues; the denial is audited.
#[tokio::test]
async fn offline_device_refuses_session() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path(), AuthMode::Password);
    let cookie = operator_cookie(&state, "op", device_api::Role::Operator);
    let _token = add_device(&state, "dev-1"); // enrolled, never connects
    install_bundle(&state, "dev-1", &[(TERMINAL_CONFIG_PATH, ENABLED_TERMINAL_YAML)]);
    let addr = serve(app).await;

    assert_refused(
        terminal_request(addr, "dev-1", Some(&cookie)),
        503,
        "offline device must refuse immediately",
    )
    .await;

    let rows = audit_rows(&state, "dev-1");
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].close_reason.as_deref().unwrap().contains("offline"),
        "{:?}",
        rows[0].close_reason
    );
}

/// Both sides check (§5.2): the server's render says enabled, but the
/// agent refuses (its converged state disagrees) — the session is
/// denied, audited with the agent's reason, and the UI leg closes.
#[tokio::test]
async fn agent_reject_denies_session() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path(), AuthMode::Password);
    let cookie = operator_cookie(&state, "op", device_api::Role::Operator);
    let token = add_device(&state, "dev-1");
    install_bundle(&state, "dev-1", &[(TERMINAL_CONFIG_PATH, ENABLED_TERMINAL_YAML)]);
    let addr = serve(app).await;
    let mut events = state.events.subscribe(None).rx;

    let mut agent = connect_agent(addr, &token).await;
    let (mut ui, _resp) =
        tokio_tungstenite::connect_async(terminal_request(addr, "dev-1", Some(&cookie)))
            .await
            .expect("operator websocket");

    // The agent's own §5.2 check refuses (same reason string as
    // reeve-agent's ext/terminal.rs).
    let open = recv_control(&mut agent).await;
    let ControlFrame::Open { id, .. } = open else {
        panic!("expected open, got {open:?}");
    };
    send_control(
        &mut agent,
        &ControlFrame::Reject {
            id,
            reason: "terminal not enabled in desired state (spec/reeve/03-terminal.md §5.2)"
                .into(),
        },
    )
    .await;

    // The UI leg is closed by the bridge.
    let closed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match ui.next().await {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(_)) => break,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "UI leg must close on agent reject");

    wait_for(
        || {
            audit_rows(&state, "dev-1")
                .first()
                .is_some_and(|r| r.ended_at.is_some())
        },
        "denied audit row finalized",
    )
    .await;
    let rows = audit_rows(&state, "dev-1");
    assert_eq!(rows.len(), 1);
    assert!(rows[0].opened_at.is_none(), "session never opened");
    assert!(
        rows[0].close_reason.as_deref().unwrap().contains("not enabled"),
        "{:?}",
        rows[0].close_reason
    );
    assert_eq!((rows[0].bytes_up, rows[0].bytes_down), (0, 0));

    // Event trail: requested -> denied.
    let mut phases = Vec::new();
    while phases.len() < 2 {
        let ev = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
            .await
            .expect("terminal-session events")
            .unwrap();
        if let SseEvent::TerminalSession(t) = ev.event {
            phases.push(t.phase);
        }
    }
    assert_eq!(phases, [TerminalPhase::Requested, TerminalPhase::Denied]);
}

/// §5.4 crash recovery (Law 3): rows left dangling by a kill -9 are
/// finalized at next startup as close_reason = server-restart.
#[tokio::test]
async fn startup_finalizes_dangling_sessions() {
    let dir = tempfile::tempdir().unwrap();
    {
        let (_app, state) = app(dir.path(), AuthMode::Password);
        let conn = state.db.lock().unwrap();
        conn.execute(
            "INSERT INTO terminal_sessions
                 (session_id, device_id, username, started_at, opened_at)
             VALUES ('ts-dangling', 'dev-1', 'op', 100, 101)",
            [],
        )
        .unwrap();
        // Process "dies" here — no finalization (kill -9).
    }
    // Restart: bootstrap on the same data dir IS the recovery path.
    let (_app, state) = app(dir.path(), AuthMode::Password);
    let rows = audit_rows(&state, "dev-1");
    assert_eq!(rows.len(), 1);
    assert!(rows[0].ended_at.is_some(), "startup must finalize the row");
    assert_eq!(rows[0].close_reason.as_deref(), Some("server-restart"));
}

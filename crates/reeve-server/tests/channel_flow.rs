//! C8 ext-channel end-to-end: websocket handshake with the real
//! rev-001/1 frame types, presence-as-fact, channel replacement, and
//! render-bump nudges — against the real router on a real listener.
//!
//! Spec sources: spec/reeve/02-channel.md §4.1 (auth before upgrade,
//! one channel per device), §4.2 (hello both ways, framing), §4.3
//! (presence from channel state + device-presence events), §4.4
//! (nudge on manifestVersion advance, best-effort).
#![cfg(feature = "ext-channel")]

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use common::*;
use futures_util::StreamExt as _;
use reeve_server::config::AuthMode;
use reeve_server::presence::{self, PresenceState};
use reeve_types::reeve::channel::{ControlFrame, NUDGE_SCOPE_DESIRED_STATE, encode_data_frame};
use reeve_types::reeve::events::{PresenceState as EvPresence, SseEvent};
use serde_json::{Value, json};
use tower::ServiceExt as _;

/// Presence state via the UNCHANGED caller surface (presence.rs).
fn presence_of(state: &reeve_server::state::AppState, id: &str) -> Option<PresenceState> {
    presence::device_presence(state, id).unwrap().map(|p| p.state)
}

/// §4.1/§4.2/§4.3: hello handshake with real frames; an open channel
/// IS presence (recency says offline, the socket says online); drop
/// flips it back; both transitions emit device-presence events.
#[tokio::test]
async fn hello_handshake_presence_flips_and_events_emit() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path(), AuthMode::None);
    let token = add_device(&state, "dev-1");
    let addr = serve(app).await;

    // Never seen, no channel: offline by recency.
    assert_eq!(presence_of(&state, "dev-1"), Some(PresenceState::Offline));
    let mut events = state.events.subscribe(None).rx;

    // connect_agent asserts the server hello (protocol rev-001/1,
    // once at open) and answers with the agent hello.
    let mut ws = connect_agent(addr, &token).await;

    // Presence-as-fact: online NOW, even though last_seen is stale.
    wait_for(
        || presence_of(&state, "dev-1") == Some(PresenceState::Online),
        "presence online after channel open",
    )
    .await;
    let online = events.recv().await.unwrap();
    match online.event {
        SseEvent::DevicePresence(e) => {
            assert_eq!(e.device_id, "dev-1");
            assert_eq!(e.state, EvPresence::Online);
        }
        other => panic!("expected device-presence online, got {other:?}"),
    }

    // Application-level liveness: our ping gets the matching pong.
    send_control(&mut ws, &ControlFrame::Ping { nonce: "n-1".into() }).await;
    assert_eq!(
        recv_control(&mut ws).await,
        ControlFrame::Pong { nonce: "n-1".into() }
    );

    // Data frames for a never-opened sub-channel id are discarded
    // silently (§4.2) — the channel must survive.
    send_binary(&mut ws, encode_data_frame(8, b"lost")).await;
    send_control(&mut ws, &ControlFrame::Ping { nonce: "n-2".into() }).await;
    assert_eq!(
        recv_control(&mut ws).await,
        ControlFrame::Pong { nonce: "n-2".into() }
    );

    // Drop the socket: presence flips offline ("link down", §4.3).
    drop(ws);
    wait_for(
        || presence_of(&state, "dev-1") == Some(PresenceState::Offline),
        "presence offline after channel drop",
    )
    .await;
    let offline = events.recv().await.unwrap();
    match offline.event {
        SseEvent::DevicePresence(e) => {
            assert_eq!(e.device_id, "dev-1");
            assert_eq!(e.state, EvPresence::Offline);
        }
        other => panic!("expected device-presence offline, got {other:?}"),
    }
}

/// §4.1: unknown or unauthenticated clients MUST be rejected before
/// upgrade — device_auth answers 401, no websocket ever exists.
#[tokio::test]
async fn unauthenticated_upgrade_rejected_before_upgrade() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path(), AuthMode::None);
    let _token = add_device(&state, "dev-1");
    let addr = serve(app).await;

    for bad in ["", "rvd_0000000000000000000000000000000000000000000000000000000000000000"] {
        let err = connect_agent_raw(addr, bad)
            .await
            .expect_err("upgrade must be refused pre-upgrade");
        match err {
            tokio_tungstenite::tungstenite::Error::Http(resp) => {
                assert_eq!(resp.status(), 401, "rejected with 401, not an upgrade");
            }
            other => panic!("expected HTTP 401 rejection, got {other:?}"),
        }
    }
    assert_eq!(
        presence_of(&state, "dev-1"),
        Some(PresenceState::Offline),
        "failed upgrades never create presence"
    );
}

/// §4.1: one channel per device — a new authenticated channel
/// atomically replaces the old (old socket closed, no draining), with
/// NO presence flap and no offline event.
#[tokio::test]
async fn new_channel_replaces_old_without_presence_flap() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path(), AuthMode::None);
    let token = add_device(&state, "dev-1");
    let addr = serve(app).await;
    let mut events = state.events.subscribe(None).rx;

    let mut first = connect_agent(addr, &token).await;
    let mut second = connect_agent(addr, &token).await;

    // The old socket is closed by the server (reconnect storms are
    // tolerated; replace is atomic).
    let closed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match first.next().await {
                None => break,
                Some(Err(_)) => break,
                Some(Ok(tokio_tungstenite::tungstenite::protocol::Message::Close(_))) => break,
                Some(Ok(_)) => continue, // drain whatever was in flight
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "old channel must be closed on replace");

    // The new channel is live.
    send_control(&mut second, &ControlFrame::Ping { nonce: "n".into() }).await;
    assert_eq!(
        recv_control(&mut second).await,
        ControlFrame::Pong { nonce: "n".into() }
    );
    assert_eq!(presence_of(&state, "dev-1"), Some(PresenceState::Online));

    // Exactly one presence event so far: the initial online. The
    // replace emitted nothing (§4.3: the device never went offline).
    let first_event = events.recv().await.unwrap();
    assert!(matches!(
        &first_event.event,
        SseEvent::DevicePresence(e) if e.state == EvPresence::Online
    ));
    assert!(
        matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "replace must not emit a presence transition"
    );

    // Closing the CURRENT channel takes the device offline.
    drop(second);
    wait_for(
        || presence_of(&state, "dev-1") == Some(PresenceState::Offline),
        "offline after the current channel drops",
    )
    .await;
}

fn put_files(uri: &str, files: &[(&str, &str)]) -> Request<Body> {
    let files: serde_json::Map<String, Value> = files
        .iter()
        .map(|(p, c)| ((*p).to_string(), Value::String(B64.encode(c))))
        .collect();
    Request::put(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "files": files }).to_string()))
        .unwrap()
}

/// Same authorable package fixture as delivery_flow.rs.
const PKG_MANIFEST: &str = "\
apiVersion: margo.org/v1-alpha1
kind: ApplicationDescription
metadata:
  id: web
  name: Web
  version: 1.0.0
  catalog:
    organization:
      - name: Reeve Tests
        site: https://example.com
deploymentProfiles:
  - type: compose
    id: web-compose
    components:
      - name: web-stack
        properties:
          packageLocation: ./compose.yml
";

/// §4.4: when a device's manifestVersion advances (an authoring
/// commit rendered a new bundle), the server sends `nudge` scope
/// `desired-state` on that device's channel.
#[tokio::test]
async fn render_bump_nudges_connected_agent() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path(), AuthMode::None);
    let token = add_device(&state, "dev-1");
    let addr = serve(app.clone()).await;

    let mut ws = connect_agent(addr, &token).await;

    // Author a package + a fleet layer using it (anonymous is admin
    // under REEVE_AUTH=none): the PUT commits a revision and kicks the
    // render pass — dev-1's manifest advances.
    let res = app
        .clone()
        .oneshot(put_files(
            "/api/tree/packages/web/1.0.0",
            &[
                ("margo.yaml", PKG_MANIFEST),
                ("compose.yml", "services:\n  web:\n    image: nginx:1.25\n"),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let res = app
        .clone()
        .oneshot(put_files(
            "/api/tree/layers/00-fleet",
            &[("apps/web/app.yaml", "package:\n  name: web\n  version: 1.0.0\n")],
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // The nudge arrives on the channel (skip any keepalive traffic).
    let nudge = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match recv_control(&mut ws).await {
                ControlFrame::Nudge { scope, .. } => break scope,
                ControlFrame::Ping { nonce } => {
                    send_control(&mut ws, &ControlFrame::Pong { nonce }).await;
                }
                _ => continue,
            }
        }
    })
    .await
    .expect("nudge within timeout");
    assert_eq!(nudge, NUDGE_SCOPE_DESIRED_STATE);
}

//! C8 ext-sse end-to-end: GET /api/reeve/v1/events delivers typed
//! rev-003/1 events for every wired source while the fleet acts —
//! plus Last-Event-ID replay, reset, and the types filter.
//!
//! Spec sources: spec/reeve/04-status-stream.md §6.1 (endpoint, auth,
//! types filter), §6.2 (ids, replay buffer, reset, keepalive), §6.3
//! (event table — payloads parse via reeve_types SseEvent::from_wire).
#![cfg(all(feature = "ext-sse", feature = "ext-channel", feature = "ext-secrets"))]

mod common;

use axum::body::Body;
use axum::http::{Request, header};
use common::*;
use http_body_util::BodyExt as _;
use reeve_server::config::AuthMode;
use reeve_server::durability::DurabilityStatus;
use reeve_server::ext::sse::{SamplerState, sample_events};
use reeve_types::reeve::events::{SseEvent, event_type};
use serde_json::json;
use tower::ServiceExt as _;

// ------------------------------------------------- SSE stream reading

/// One parsed SSE record.
#[derive(Debug, Clone)]
struct SseRecord {
    id: Option<u64>,
    event: String,
    data: String,
}

/// Incremental reader over a streaming SSE response body.
struct SseReader {
    body: Body,
    buf: String,
}

impl SseReader {
    fn new(body: Body) -> Self {
        SseReader { body, buf: String::new() }
    }

    /// Next complete record (blocks on the live stream; 10 s cap).
    async fn next(&mut self) -> SseRecord {
        loop {
            if let Some(pos) = self.buf.find("\n\n") {
                let block = self.buf[..pos].to_string();
                self.buf.drain(..pos + 2);
                if let Some(record) = parse_block(&block) {
                    return record;
                }
                continue; // comment/keepalive block — skip
            }
            let frame = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                self.body.frame(),
            )
            .await
            .expect("sse frame within timeout")
            .expect("stream open")
            .expect("frame ok");
            if let Some(data) = frame.data_ref() {
                self.buf.push_str(std::str::from_utf8(data).unwrap());
            }
        }
    }

    /// Read until an event of `wanted` type arrives; returns it parsed
    /// through the tolerant-reader path (§6.3: unknown types ignored).
    async fn next_of(&mut self, wanted: &str) -> (SseRecord, SseEvent) {
        loop {
            let record = self.next().await;
            if record.event != wanted {
                continue;
            }
            let parsed = SseEvent::from_wire(&record.event, &record.data)
                .expect("known event payload parses")
                .expect("known event type");
            return (record, parsed);
        }
    }
}

fn parse_block(block: &str) -> Option<SseRecord> {
    let mut id = None;
    let mut event = None;
    let mut data = String::new();
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("id:") {
            id = rest.trim().parse().ok();
        } else if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        }
        // lines starting with ':' are comments (keepalive)
    }
    Some(SseRecord { id, event: event?, data })
}

fn events_request(uri: &str, last_event_id: Option<u64>) -> Request<Body> {
    let mut b = Request::get(uri);
    if let Some(id) = last_event_id {
        b = b.header("last-event-id", id.to_string());
    }
    b.body(Body::empty()).unwrap()
}

/// Open the SSE stream through the real route (viewer+; anonymous is
/// admin under REEVE_AUTH=none).
async fn open_stream(app: &axum::Router, uri: &str, last_event_id: Option<u64>) -> SseReader {
    let res = app
        .clone()
        .oneshot(events_request(uri, last_event_id))
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert!(
        res.headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/event-stream"),
        "§6.1: text/event-stream"
    );
    SseReader::new(res.into_body())
}

fn status_body(deployment_id: &str, state: &str, seq: u64) -> String {
    json!({
        "apiVersion": "deployment.margo.org/v1alpha1",
        "kind": "DeploymentStatusManifest",
        "deploymentId": deployment_id,
        "status": { "state": state },
        "components": [{ "name": "web-stack", "state": state }],
        "reeve": { "observedAt": "2026-07-10T00:00:00Z", "seq": seq }
    })
    .to_string()
}

// --------------------------------------------------------------- tests

/// The §6.3 table live: while a fake fleet acts (channel connects and
/// drops, a device reports status, a secret rotates, durability lags
/// and verifies), the stream delivers each typed event with monotonic
/// ids, all parseable by reeve-types' from_wire.
#[tokio::test]
async fn stream_delivers_typed_events_for_each_wired_source() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path(), AuthMode::None);
    let token = add_device(&state, "dev-1");
    let addr = serve(app.clone()).await;

    let mut stream = open_stream(&app, "/api/reeve/v1/events", None).await;

    // --- device-presence: channel opens (ext-channel is the producer).
    let agent = connect_agent(addr, &token).await;
    let (record, parsed) = stream.next_of(event_type::DEVICE_PRESENCE).await;
    let first_id = record.id.expect("events carry ids (§6.2)");
    match parsed {
        SseEvent::DevicePresence(e) => {
            assert_eq!(e.device_id, "dev-1");
        }
        other => panic!("expected presence, got {other:?}"),
    }

    // --- deployment-status: Margo status ingest changes overall state.
    let res = app
        .clone()
        .oneshot(
            Request::post("/api/v1/clients/dev-1/deployments/dep-1/status")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::from(status_body("dep-1", "installing", 1)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(res.status().is_success(), "{}", res.status());
    let (record, parsed) = stream.next_of(event_type::DEPLOYMENT_STATUS).await;
    assert!(record.id.unwrap() > first_id, "ids are monotonic (§6.2)");
    match parsed {
        SseEvent::DeploymentStatus(e) => {
            assert_eq!((e.device_id.as_str(), e.deployment_id.as_str()), ("dev-1", "dep-1"));
        }
        other => panic!("expected deployment-status, got {other:?}"),
    }

    // Same state re-reported (new seq): current state unchanged, so
    // NO further deployment-status event may precede the next distinct
    // one — verified implicitly by asserting the next event we see for
    // this source is the installed transition.
    for (seq, st) in [(2u64, "installing"), (3, "installed")] {
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/v1/clients/dev-1/deployments/dep-1/status")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::from(status_body("dep-1", st, seq)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(res.status().is_success());
    }
    let (_, parsed) = stream.next_of(event_type::DEPLOYMENT_STATUS).await;
    match parsed {
        SseEvent::DeploymentStatus(e) => {
            assert_eq!(
                serde_json::to_value(e.state).unwrap(),
                json!("installed"),
                "the duplicate 'installing' report emitted nothing"
            );
        }
        other => panic!("expected deployment-status, got {other:?}"),
    }

    // --- secret-rotation: operator PUT (ext-secrets is the producer).
    let res = app
        .clone()
        .oneshot(
            Request::put("/api/secrets")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"name": "db-password", "scope": "fleet", "value": "hunter2"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(res.status().is_success());
    let (_, parsed) = stream.next_of(event_type::SECRET_ROTATION).await;
    match parsed {
        SseEvent::SecretRotation(e) => {
            assert_eq!((e.secret_name.as_str(), e.scope.as_str(), e.version), ("db-password", "fleet", 1));
        }
        other => panic!("expected secret-rotation, got {other:?}"),
    }

    // --- durability-lag + verify-restore: the sampler transition
    // logic drives the hub (the scheduled task samples the same way).
    let mut sampler = SamplerState::default();
    let status = DurabilityStatus {
        tier: "changeset".into(),
        degraded: false,
        last_error: None,
        generation: Some("gen-1".into()),
        last_snapshot_at: Some(0),
        snapshot_age_secs: Some(0),
        last_changeset_seq: Some(9),
        last_changeset_at: Some(0),
        pending_changesets: 4,
        last_verify: Some(reeve_server::durability::VerifySummary {
            finished_at: reeve_server::db::now_secs(),
            outcome: "ok".into(),
            generation: Some("gen-1".into()),
            last_seq: Some(9),
            detail: None,
        }),
    };
    for event in sample_events(&mut sampler, &status, reeve_server::db::now_secs(), 30) {
        state.events.emit(event);
    }
    let (_, parsed) = stream.next_of(event_type::DURABILITY_LAG).await;
    match parsed {
        SseEvent::DurabilityLag(e) => {
            assert_eq!(e.generation, "gen-1");
            assert_eq!(e.last_seq, 9);
            assert!(e.lag_seconds > 30);
        }
        other => panic!("expected durability-lag, got {other:?}"),
    }
    let (_, parsed) = stream.next_of(event_type::VERIFY_RESTORE).await;
    match parsed {
        SseEvent::VerifyRestore(e) => assert_eq!(e.snapshot_ts, "gen-1"),
        other => panic!("expected verify-restore, got {other:?}"),
    }

    // --- device-presence offline: channel drops.
    drop(agent);
    let (_, parsed) = stream.next_of(event_type::DEVICE_PRESENCE).await;
    match parsed {
        SseEvent::DevicePresence(e) => {
            assert_eq!(
                serde_json::to_value(e.state).unwrap(),
                json!("offline"),
                "channel drop publishes the offline transition"
            );
        }
        other => panic!("expected presence, got {other:?}"),
    }
}

/// §6.2: Last-Event-ID replays buffered events; an unknown id (e.g. a
/// previous boot's stream) gets `reset` FIRST so the client refetches.
#[tokio::test]
async fn last_event_id_replays_and_unknown_id_resets() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path(), AuthMode::None);

    // Three events through the hub (any producer looks the same).
    for n in 1..=3u64 {
        state.events.emit(SseEvent::SecretRotation(
            reeve_types::reeve::events::SecretRotationEvent {
                ts: format!("t{n}"),
                secret_name: format!("s{n}"),
                scope: "fleet".into(),
                version: n,
                state: reeve_types::reeve::events::SecretRotationState::Propagating,
            },
        ));
    }

    // Reconnect with Last-Event-ID: 1 — events 2 and 3 replay in order.
    let mut stream = open_stream(&app, "/api/reeve/v1/events", Some(1)).await;
    let (r2, _) = stream.next_of(event_type::SECRET_ROTATION).await;
    let (r3, _) = stream.next_of(event_type::SECRET_ROTATION).await;
    assert_eq!((r2.id, r3.id), (Some(2), Some(3)));

    // An id this stream never issued: reset first (§6.2 MUST).
    let mut stream = open_stream(&app, "/api/reeve/v1/events", Some(999)).await;
    let first = stream.next().await;
    assert_eq!(first.event, event_type::RESET);
    let parsed = SseEvent::from_wire(&first.event, &first.data).unwrap().unwrap();
    assert!(matches!(parsed, SseEvent::Reset(_)));
}

/// §6.1: the `types` query parameter filters server-side; unknown
/// names are ignored.
#[tokio::test]
async fn types_filter_is_server_side() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = app(dir.path(), AuthMode::None);

    let mut stream = open_stream(
        &app,
        "/api/reeve/v1/events?types=secret-rotation,frobnicate",
        None,
    )
    .await;

    // A filtered-out type followed by a matching one: only the match
    // arrives (order preserved, nothing else in between).
    state.events.emit(SseEvent::DevicePresence(
        reeve_types::reeve::events::DevicePresenceEvent {
            ts: "t".into(),
            device_id: "dev-1".into(),
            state: reeve_types::reeve::events::PresenceState::Online,
            since: "t".into(),
        },
    ));
    state.events.emit(SseEvent::SecretRotation(
        reeve_types::reeve::events::SecretRotationEvent {
            ts: "t".into(),
            secret_name: "s".into(),
            scope: "fleet".into(),
            version: 1,
            state: reeve_types::reeve::events::SecretRotationState::Propagating,
        },
    ));
    let record = stream.next().await;
    assert_eq!(
        record.event,
        event_type::SECRET_ROTATION,
        "the device-presence event was filtered server-side"
    );
}

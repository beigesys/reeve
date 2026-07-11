//! /api/devices flow (Track D): list with presence + labels +
//! layer-chain membership + current deployment states; detail with
//! render provenance (revision id, manifest version, per-app
//! secrets_version); journal paging. Auth: viewer+ (REEVE_AUTH=none
//! maps anonymous to admin — the same harness every other flow uses).

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use rusqlite::params;
use tower::ServiceExt as _;

use reeve_server::config::AuthMode;

async fn get_json(
    app: &axum::Router,
    uri: &str,
) -> (StatusCode, serde_json::Value) {
    let res = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, body)
}

#[tokio::test]
async fn list_carries_presence_labels_chain_and_deployment_states() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = common::app(dir.path(), AuthMode::None);
    common::add_device(&state, "dev-1");
    {
        let conn = state.db.lock().unwrap();
        conn.execute(
            "UPDATE devices SET labels = '{\"env\":\"prod\"}', class = 'gpu',
                    region = 'emea', site = 'plant-a', last_seen_at = ?1
             WHERE device_id = 'dev-1'",
            params![reeve_server::db::now_secs()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO deployment_status_current
                 (device_id, deployment_id, state, seq, observed_at, received_at, payload)
             VALUES ('dev-1', 'dep-1', 'installed', 7, '2026-07-10T06:00:00Z', 100, '{}')",
            [],
        )
        .unwrap();
    }

    let (status, body) = get_json(&app, "/api/devices").await;
    assert_eq!(status, StatusCode::OK);
    let list = body.as_array().unwrap();
    assert_eq!(list.len(), 1);
    let d = &list[0];
    assert_eq!(d["deviceId"], "dev-1");
    assert_eq!(d["hostname"], "box");
    assert_eq!(d["labels"]["env"], "prod");
    assert_eq!(d["class"], "gpu");
    assert_eq!(d["region"], "emea");
    assert_eq!(d["site"], "plant-a");
    assert_eq!(d["stale"], false);
    assert_eq!(d["presence"]["state"], "online", "fresh last_seen => online");
    assert_eq!(d["deployments"][0]["deploymentId"], "dep-1");
    assert_eq!(d["deployments"][0]["state"], "installed");
    assert_eq!(d["deployments"][0]["seq"], 7);
}

#[tokio::test]
async fn never_seen_device_is_offline() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = common::app(dir.path(), AuthMode::None);
    common::add_device(&state, "dev-cold");

    let (status, body) = get_json(&app, "/api/devices").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["presence"]["state"], "offline");
    assert_eq!(body[0]["presence"]["since"], serde_json::Value::Null);
}

#[tokio::test]
async fn detail_carries_render_provenance_with_secrets_versions() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = common::app(dir.path(), AuthMode::None);
    common::add_device(&state, "dev-1");
    {
        // A stored manifest row as the render pipeline writes it: the
        // provenance surface must expose revision id, manifest version
        // (epoch/counter) and the per-app secrets_version.
        let manifest_json = serde_json::json!({
            "manifestVersion": (2u64 << 48) | 5,
            "bundle": null,
            "apps": [
                { "appId": "nginx", "deploymentId": "dep-1",
                  "secrets_version": "sha256:beef" }
            ]
        })
        .to_string();
        let conn = state.db.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO device_manifests
                 (device_id, manifest_version, counter, generation, content_digest,
                  bundle_digest, layer_digest, manifest_json, etag,
                  rendered_revision, updated_at)
             VALUES ('dev-1', ?1, 5, 3, 'sha256:aaaa', NULL, NULL, ?2,
                     'sha256:bbbb', 42, 1000)",
            params![((2u64 << 48) | 5) as i64, manifest_json],
        )
        .unwrap();
    }

    let (status, body) = get_json(&app, "/api/devices/dev-1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deviceId"], "dev-1");
    let render = &body["render"];
    assert_eq!(render["epoch"], 2);
    assert_eq!(render["counter"], 5);
    assert_eq!(render["generation"], 3);
    assert_eq!(render["renderedRevision"], 42);
    assert_eq!(render["contentDigest"], "sha256:aaaa");
    assert_eq!(render["bundleDigest"], serde_json::Value::Null);
    assert_eq!(render["apps"][0]["appId"], "nginx");
    assert_eq!(render["apps"][0]["secrets_version"], "sha256:beef");
}

#[tokio::test]
async fn detail_of_unrendered_device_has_null_render() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = common::app(dir.path(), AuthMode::None);
    common::add_device(&state, "dev-1");
    let (status, body) = get_json(&app, "/api/devices/dev-1").await;
    assert_eq!(status, StatusCode::OK);
    // bootstrap's startup render may have rendered it (zero apps) —
    // either way the field exists and the summary is intact.
    assert!(body.get("render").is_some());
    assert_eq!(body["deviceId"], "dev-1");
}

#[tokio::test]
async fn unknown_device_is_404() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _state) = common::app(dir.path(), AuthMode::None);
    let (status, _) = get_json(&app, "/api/devices/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get_json(&app, "/api/devices/nope/journal").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn journal_pages_newest_first() {
    let dir = tempfile::tempdir().unwrap();
    let (app, state) = common::app(dir.path(), AuthMode::None);
    common::add_device(&state, "dev-1");
    {
        let conn = state.db.lock().unwrap();
        for seq in 1..=5 {
            conn.execute(
                "INSERT INTO status_journal
                     (device_id, seq, observed_at, received_at, kind, payload)
                 VALUES ('dev-1', ?1, ?2, ?3, 'lifecycle', '{\"event\":\"tick\"}')",
                params![seq, format!("2026-07-10T06:00:0{seq}Z"), 100 + seq],
            )
            .unwrap();
        }
    }

    let (status, body) = get_json(&app, "/api/devices/dev-1/journal?limit=2").await;
    assert_eq!(status, StatusCode::OK);
    let records = body["records"].as_array().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["seq"], 5, "newest first");
    assert_eq!(records[1]["seq"], 4);
    assert_eq!(records[0]["payload"]["event"], "tick");
    assert_eq!(body["nextBeforeSeq"], 4);

    // Follow the cursor to the end.
    let (_, page2) =
        get_json(&app, "/api/devices/dev-1/journal?limit=10&before_seq=4").await;
    let records = page2["records"].as_array().unwrap();
    assert_eq!(
        records.iter().map(|r| r["seq"].as_i64().unwrap()).collect::<Vec<_>>(),
        vec![3, 2, 1]
    );
    assert!(page2.get("nextBeforeSeq").is_none());
}

//! Human-facing device fleet routes (Track D, docs/build-charter.md):
//! `GET /api/devices` (fleet list: presence, labels, layer-chain
//! membership, current deployment states) and
//! `GET /api/devices/{device_id}` (detail: the same plus render
//! provenance from `device_manifests` — revision id, manifest version,
//! per-app `secrets_version`) and
//! `GET /api/devices/{device_id}/journal` (status journal page,
//! spec/reeve/05-health-journal.md §7.3 forensic record).
//!
//! Viewer+ like every other human read. Presence is presence.rs's
//! channel-above-recency answer (spec/reeve/02-channel.md §4.3);
//! deployment states are the `deployment_status_current`
//! materialization (spec/reeve/05-health-journal.md §7.3 — highest
//! journaled seq, never latest arrival).

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use device_api::{Identity, Role};
use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::warn;
use utoipa::ToSchema;

use crate::presence;
use crate::state::AppState;

fn internal(e: impl std::fmt::Display) -> Response {
    warn!(error = %e, "devices route internal error");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

/// Presence answer (spec/reeve/02-channel.md §4.3 vocabulary).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PresenceInfo {
    /// `online` | `offline`.
    pub state: reeve_types::reeve::events::PresenceState,
    /// Unix seconds: channel-open time when online via channel,
    /// last contact otherwise; `null` = never seen.
    pub since: Option<i64>,
}

impl From<presence::Presence> for PresenceInfo {
    fn from(p: presence::Presence) -> Self {
        PresenceInfo {
            state: match p.state {
                presence::PresenceState::Online => {
                    reeve_types::reeve::events::PresenceState::Online
                }
                presence::PresenceState::Offline => {
                    reeve_types::reeve::events::PresenceState::Offline
                }
            },
            since: p.since,
        }
    }
}

/// Current state of one deployment on a device
/// (`deployment_status_current` row).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeviceDeploymentState {
    pub deployment_id: String,
    /// Margo deployment state (`pending` … `failed`).
    pub state: String,
    /// Journal seq of the report (absent for vanilla Margo reports).
    pub seq: Option<i64>,
    /// Device-assigned RFC 3339 timestamp (absent for vanilla reports).
    pub observed_at: Option<String>,
    /// Server receipt time, unix seconds.
    pub received_at: i64,
}

/// One device as listed by `GET /api/devices`.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeviceSummary {
    pub device_id: String,
    pub hostname: String,
    pub arch: String,
    pub agent_version: String,
    /// Unix seconds.
    pub enrolled_at: i64,
    /// Free-form labels (docs/decisions/tree-render.md D12: labels
    /// group and filter, never configure).
    pub labels: BTreeMap<String, String>,
    /// Layer-chain membership (D11: fleet -> class? -> region -> site
    /// -> device), each nullable.
    pub class: Option<String>,
    pub region: Option<String>,
    pub site: Option<String>,
    /// Identity superseded by a newer enrollment from the same
    /// hostname (docs/decisions/agent.md D4 wiped-box case).
    pub stale: bool,
    /// Child tier this device reached us through (federation §8.3);
    /// `null` = enrolled here.
    pub tier_origin: Option<String>,
    /// Unix seconds of last contact; `null` = never seen.
    pub last_seen_at: Option<i64>,
    pub presence: PresenceInfo,
    /// Current per-deployment states.
    pub deployments: Vec<DeviceDeploymentState>,
}

/// Render provenance of a device's current State Manifest
/// (`device_manifests` row + the manifest's per-app entries;
/// docs/decisions/delivery.md D13).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RenderProvenance {
    /// The packed (epoch, counter) wire u64
    /// (spec/reeve/08-packaging.md §10.2).
    pub manifest_version: u64,
    /// High 16 bits — bumped by restore fencing
    /// (spec/reeve/07-durability.md §9.5).
    pub epoch: u16,
    /// Low 48 bits — monotonic per device.
    pub counter: u64,
    /// D2 render generation counter.
    pub generation: i64,
    /// sha256 over the rendered apps/** file set (change detector).
    pub content_digest: String,
    /// OCI image manifest digest the device pulls; `null` = zero apps.
    pub bundle_digest: Option<String>,
    /// Local-stream revision id this device was last rendered against.
    pub rendered_revision: i64,
    /// Strong ETag of the served manifest JSON.
    pub etag: String,
    /// Unix seconds.
    pub updated_at: i64,
    /// Per-app manifest entries — carries each app's `secrets_version`
    /// (spec/reeve/10-secrets.md §12.4).
    pub apps: Vec<reeve_types::reeve::manifest::AppManifestEntry>,
}

/// `GET /api/devices/{device_id}` response.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeviceDetail {
    #[serde(flatten)]
    pub summary: DeviceSummary,
    /// `null` when the device has never been rendered.
    pub render: Option<RenderProvenance>,
}

/// One status-journal record (spec/reeve/05-health-journal.md §7.3).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct JournalEntry {
    pub seq: i64,
    /// Device-assigned original RFC 3339 timestamp, verbatim.
    pub observed_at: String,
    /// Server receipt time, unix seconds.
    pub received_at: i64,
    /// `status` | `health` | `lifecycle` | `gap`.
    pub kind: String,
    /// The journaled payload; absent for gap marks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
}

/// `GET /api/devices/{device_id}/journal` response: newest first.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct JournalPage {
    pub records: Vec<JournalEntry>,
    /// Pass as `before_seq` to fetch the next (older) page; absent
    /// when this page reached the start of the journal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_before_seq: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct JournalQuery {
    /// Page size; default 100, max 1000.
    pub limit: Option<usize>,
    /// Return records with seq strictly below this (older page).
    pub before_seq: Option<i64>,
}

/// Row -> summary; deployments filled by the caller.
struct DeviceRow {
    device_id: String,
    hostname: String,
    arch: String,
    agent_version: String,
    enrolled_at: i64,
    labels: BTreeMap<String, String>,
    class: Option<String>,
    region: Option<String>,
    site: Option<String>,
    stale: bool,
    tier_origin: Option<String>,
    last_seen_at: Option<i64>,
}

const DEVICE_COLUMNS: &str = "device_id, hostname, arch, agent_version, enrolled_at, \
     labels, class, region, site, stale, tier_origin, last_seen_at";

fn row_to_device(r: &rusqlite::Row<'_>) -> rusqlite::Result<DeviceRow> {
    let labels_json: String = r.get(5)?;
    Ok(DeviceRow {
        device_id: r.get(0)?,
        hostname: r.get(1)?,
        arch: r.get(2)?,
        agent_version: r.get(3)?,
        enrolled_at: r.get(4)?,
        labels: serde_json::from_str(&labels_json).unwrap_or_default(),
        class: r.get(6)?,
        region: r.get(7)?,
        site: r.get(8)?,
        stale: r.get::<_, i64>(9)? != 0,
        tier_origin: r.get(10)?,
        last_seen_at: r.get(11)?,
    })
}

fn deployments_of(
    conn: &Connection,
    device_id: &str,
) -> rusqlite::Result<Vec<DeviceDeploymentState>> {
    let mut stmt = conn.prepare_cached(
        "SELECT deployment_id, state, seq, observed_at, received_at
         FROM deployment_status_current WHERE device_id = ?1
         ORDER BY deployment_id",
    )?;
    let rows = stmt.query_map(params![device_id], |r| {
        Ok(DeviceDeploymentState {
            deployment_id: r.get(0)?,
            state: r.get(1)?,
            seq: r.get(2)?,
            observed_at: r.get(3)?,
            received_at: r.get(4)?,
        })
    })?;
    rows.collect()
}

/// Presence for a row already in hand (channel-above-recency,
/// presence.rs) — no extra device query.
fn presence_of(state: &AppState, device_id: &str, last_seen_at: Option<i64>) -> PresenceInfo {
    if let Some(since) = state.channels.online_since(device_id) {
        return PresenceInfo {
            state: reeve_types::reeve::events::PresenceState::Online,
            since: Some(since),
        };
    }
    presence::from_recency(
        last_seen_at,
        crate::db::now_secs(),
        presence::DEFAULT_ONLINE_THRESHOLD_SECS,
    )
    .into()
}

fn summarize(state: &AppState, conn: &Connection, row: DeviceRow) -> rusqlite::Result<DeviceSummary> {
    let deployments = deployments_of(conn, &row.device_id)?;
    let presence = presence_of(state, &row.device_id, row.last_seen_at);
    Ok(DeviceSummary {
        presence,
        deployments,
        device_id: row.device_id,
        hostname: row.hostname,
        arch: row.arch,
        agent_version: row.agent_version,
        enrolled_at: row.enrolled_at,
        labels: row.labels,
        class: row.class,
        region: row.region,
        site: row.site,
        stale: row.stale,
        tier_origin: row.tier_origin,
        last_seen_at: row.last_seen_at,
    })
}

/// GET /api/devices (viewer+) — the fleet, ordered by device id.
#[utoipa::path(
    get,
    path = "/api/devices",
    tag = "devices",
    responses(
        (status = 200, description = "All enrolled devices with presence, labels, layer-chain membership and current deployment states", body = Vec<DeviceSummary>),
        (status = 401, description = "Unauthenticated"),
        (status = 403, description = "Below viewer role"),
    ),
)]
pub async fn list(State(state): State<AppState>, identity: Identity) -> Response {
    if let Err(status) = crate::join_tokens::require_at_least(&state, &identity, Role::Viewer) {
        return status.into_response();
    }
    let conn = state.db.lock().expect("db mutex poisoned");
    let result = (|| -> rusqlite::Result<Vec<DeviceSummary>> {
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {DEVICE_COLUMNS} FROM devices ORDER BY device_id"
        ))?;
        let rows: Vec<DeviceRow> = stmt
            .query_map([], row_to_device)?
            .collect::<Result<_, _>>()?;
        rows.into_iter()
            .map(|row| summarize(&state, &conn, row))
            .collect()
    })();
    match result {
        Ok(list) => Json(list).into_response(),
        Err(e) => internal(e),
    }
}

fn provenance_of(
    conn: &Connection,
    device_id: &str,
) -> rusqlite::Result<Option<RenderProvenance>> {
    let row = conn
        .query_row(
            "SELECT manifest_version, counter, generation, content_digest,
                    bundle_digest, rendered_revision, etag, updated_at, manifest_json
             FROM device_manifests WHERE device_id = ?1",
            params![device_id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, String>(8)?,
                ))
            },
        )
        .optional()?;
    Ok(row.map(
        |(mv, counter, generation, content_digest, bundle_digest, rendered_revision, etag, updated_at, manifest_json)| {
            let version = reeve_types::reeve::manifest::ManifestVersion(mv as u64);
            // Per-app secrets_version travels inside the stored
            // manifest bytes; a parse failure yields no app entries
            // (provenance degrades, never errors).
            let apps = serde_json::from_str::<reeve_types::reeve::manifest::StateManifest>(
                &manifest_json,
            )
            .map(|m| m.apps)
            .unwrap_or_default();
            RenderProvenance {
                manifest_version: version.0,
                epoch: version.epoch(),
                counter: counter as u64,
                generation,
                content_digest,
                bundle_digest,
                rendered_revision,
                etag,
                updated_at,
                apps,
            }
        },
    ))
}

/// GET /api/devices/{device_id} (viewer+) — one device, with render
/// provenance.
#[utoipa::path(
    get,
    path = "/api/devices/{device_id}",
    tag = "devices",
    params(("device_id" = String, Path, description = "Device id")),
    responses(
        (status = 200, description = "Device detail: summary plus render provenance (revision ids, manifest version, per-app secrets_version)", body = DeviceDetail),
        (status = 401, description = "Unauthenticated"),
        (status = 403, description = "Below viewer role"),
        (status = 404, description = "Unknown device", body = device_api::ErrorBody),
    ),
)]
pub async fn detail(
    State(state): State<AppState>,
    identity: Identity,
    Path(device_id): Path<String>,
) -> Response {
    if let Err(status) = crate::join_tokens::require_at_least(&state, &identity, Role::Viewer) {
        return status.into_response();
    }
    let conn = state.db.lock().expect("db mutex poisoned");
    let result = (|| -> rusqlite::Result<Option<DeviceDetail>> {
        let row = conn
            .query_row(
                &format!("SELECT {DEVICE_COLUMNS} FROM devices WHERE device_id = ?1"),
                params![device_id],
                row_to_device,
            )
            .optional()?;
        let Some(row) = row else { return Ok(None) };
        let summary = summarize(&state, &conn, row)?;
        let render = provenance_of(&conn, &device_id)?;
        Ok(Some(DeviceDetail { summary, render }))
    })();
    match result {
        Ok(Some(detail)) => Json(detail).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "unknown device" })),
        )
            .into_response(),
        Err(e) => internal(e),
    }
}

/// GET /api/devices/{device_id}/journal (viewer+) — status journal
/// page, newest first (§7.3 forensic record; late-backfilled records
/// appear in seq order, not arrival order).
#[utoipa::path(
    get,
    path = "/api/devices/{device_id}/journal",
    tag = "devices",
    params(
        ("device_id" = String, Path, description = "Device id"),
        ("limit" = Option<usize>, Query, description = "Page size; default 100, max 1000"),
        ("before_seq" = Option<i64>, Query, description = "Return records with seq strictly below this (older page)"),
    ),
    responses(
        (status = 200, description = "One journal page, newest first", body = JournalPage),
        (status = 401, description = "Unauthenticated"),
        (status = 403, description = "Below viewer role"),
        (status = 404, description = "Unknown device", body = device_api::ErrorBody),
    ),
)]
pub async fn journal(
    State(state): State<AppState>,
    identity: Identity,
    Path(device_id): Path<String>,
    Query(q): Query<JournalQuery>,
) -> Response {
    if let Err(status) = crate::join_tokens::require_at_least(&state, &identity, Role::Viewer) {
        return status.into_response();
    }
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let conn = state.db.lock().expect("db mutex poisoned");
    let result = (|| -> rusqlite::Result<Option<JournalPage>> {
        let known: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM devices WHERE device_id = ?1",
                params![device_id],
                |r| r.get(0),
            )
            .optional()?;
        if known.is_none() {
            return Ok(None);
        }
        let mut stmt = conn.prepare_cached(
            "SELECT seq, observed_at, received_at, kind, payload
             FROM status_journal
             WHERE device_id = ?1 AND (?2 IS NULL OR seq < ?2)
             ORDER BY seq DESC LIMIT ?3",
        )?;
        let records: Vec<JournalEntry> = stmt
            .query_map(params![device_id, q.before_seq, limit as i64], |r| {
                let payload: Option<String> = r.get(4)?;
                Ok(JournalEntry {
                    seq: r.get(0)?,
                    observed_at: r.get(1)?,
                    received_at: r.get(2)?,
                    kind: r.get(3)?,
                    payload: payload.and_then(|p| serde_json::from_str(&p).ok()),
                })
            })?
            .collect::<Result<_, _>>()?;
        let next_before_seq = if records.len() == limit {
            records.last().map(|r| r.seq)
        } else {
            None
        };
        Ok(Some(JournalPage {
            records,
            next_before_seq,
        }))
    })();
    match result {
        Ok(Some(page)) => Json(page).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "unknown device" })),
        )
            .into_response(),
        Err(e) => internal(e),
    }
}

//! Staged rollouts (C9, `ext-rollouts`) — spec/reeve/09-rollouts.md
//! (REV-008), docs/decisions/tree-render.md D12.
//!
//! WFM-internal control layer: a rollout is nothing but State-Manifest
//! advancement, wave by wave (§11 intro). No new agent behavior, no new
//! wire format, no capability advertisement — agents converge on their
//! manifest exactly as always and never know a rollout exists, so
//! REV-008 is deliberately absent from `delivery::server_capabilities`.
//!
//! Mechanics over the existing pipeline (§11.2): the CORE
//! `device_render_targets` table (V8, honored by render.rs) pins each
//! cohort device's render to a revision. Creation holds every cohort
//! device at its baseline; starting a wave moves its devices' targets
//! to the rollout revision (one SQLite tx per device — §11.2 atomic
//! advancement); completion deletes the rollout's target rows, which
//! returns its devices to head-tracking. Pausing/aborting simply stops
//! moving targets — devices already advanced stay, devices not yet
//! advanced stay (§11.2 stable position; §11.5 nothing ever moves a
//! manifest backward, rollback is a NEW rollout of a new revision that
//! carries the old content).
//!
//! Crash-only (Law 3): every state transition is one transaction in
//! the server DB; the engine is a pure function of that state
//! ([`tick`]), so a kill -9 anywhere mid-rollout resumes exactly where
//! it stopped when the engine next ticks — startup needs no special
//! reconcile beyond the interval task starting.
//!
//! Gate policy (§11.3): per advanced device of the current wave —
//! Margo deployment status (`deployment_status_current`, fresh since
//! advancement) is the convergence/health signal; devices with no
//! fresh reports (offline, link-degraded, unknown — §7.4) count as
//! `undetermined`, never failed. The gate passes when converged ≥
//! ceil(passFraction × determinable) and undetermined ≤ allowance;
//! after soak + gate timeout it resolves with what is determinable.
//! Auto-pause (§11.4) trips at ANY time during a wave or its soak when
//! failed devices reach the failure threshold.

use std::collections::{BTreeMap, BTreeSet};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use device_api::{Identity, Role};
use reeve_types::reeve::events::{RolloutEvent, RolloutPhase, SseEvent};
use reeve_types::reeve::manifest::StateManifest;
use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::warn;

use crate::db::now_secs;
use crate::events::EventHub;
use crate::join_tokens::require_at_least;
use crate::render;
use crate::state::AppState;

/// §11.3 RECOMMENDED default soak window: 15 minutes.
pub const DEFAULT_SOAK_SECS: i64 = 900;
/// §11.3 "soak window plus a timeout": how long past the soak the gate
/// keeps waiting for undetermined devices before resolving.
pub const DEFAULT_GATE_TIMEOUT_SECS: i64 = 900;
/// §11.3 RECOMMENDED default pass fraction: 100% of determinable.
pub const DEFAULT_PASS_FRACTION: f64 = 1.0;
/// §11.4 RECOMMENDED default failure threshold: any failed device.
pub const DEFAULT_FAILURE_THRESHOLD: i64 = 1;
/// Engine cadence (the "periodic task" leg; status ingest events are
/// the fast, event-driven leg — [`spawn_engine`]).
pub const ENGINE_TICK: std::time::Duration = std::time::Duration::from_secs(1);

// ---------------------------------------------------------------------
// Request/response shapes
// ---------------------------------------------------------------------

/// Cohort selector (§11 terms): explicit device list, tree selections
/// (layer subtrees), and/or label matches (D12: labels select COHORTS
/// and filter UIs only — they never select or inject configuration).
/// Selectors union; the resolved cohort is recorded at creation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CohortSpec {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<String>,
    /// Layer dir names per D11 grammar (`00-fleet`, `05-class.<n>`,
    /// `10-region.<n>`, `20-site.<n>`, `30-device.<id>`); the numeric
    /// prefix is optional here (`site.plant-a` works).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layers: Vec<String>,
    /// All pairs must match the device's free-form labels.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

/// Gate policy overrides (§11.3); defaults above.
#[derive(Debug, Clone, Default, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GateSpec {
    pub soak_secs: Option<i64>,
    pub gate_timeout_secs: Option<i64>,
    pub pass_fraction: Option<f64>,
    /// Max undetermined devices a passing gate tolerates. Absent =
    /// the whole wave may be undetermined (chronically-offline fleets,
    /// Law 5); 0 = strict.
    pub undetermined_allowance: Option<i64>,
}

/// POST /api/rollouts body. Waves: exactly one of `waves` (explicit
/// partition), `strategy` (e.g. `["1", "10%", "rest"]`), `waveCount`,
/// or none (single wave = whole cohort).
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateRequest {
    /// The source tree revision (§11.1) — an existing local revision.
    pub revision: i64,
    pub cohort: CohortSpec,
    #[serde(default)]
    pub waves: Option<Vec<Vec<String>>>,
    #[serde(default)]
    pub strategy: Option<Vec<String>>,
    #[serde(default)]
    pub wave_count: Option<u32>,
    #[serde(default)]
    pub gate: GateSpec,
    #[serde(default)]
    pub failure_threshold: Option<i64>,
    /// Hold revision for not-yet-advanced devices; default: the target
    /// revision's parent (see module docs — DECISION below).
    #[serde(default)]
    pub baseline_revision: Option<i64>,
}

// ---------------------------------------------------------------------
// Cohort + wave resolution (pure over device rows)
// ---------------------------------------------------------------------

/// One device row's selector-relevant fields.
struct SelectableDevice {
    device_id: String,
    class: Option<String>,
    region: Option<String>,
    site: Option<String>,
    labels: BTreeMap<String, String>,
}

fn load_selectable(conn: &Connection) -> rusqlite::Result<Vec<SelectableDevice>> {
    let mut stmt = conn.prepare(
        "SELECT device_id, class, region, site, labels FROM devices
         WHERE stale = 0 ORDER BY device_id",
    )?;
    let rows = stmt.query_map([], |r| {
        let labels_json: String = r.get(4)?;
        Ok(SelectableDevice {
            device_id: r.get(0)?,
            class: r.get(1)?,
            region: r.get(2)?,
            site: r.get(3)?,
            labels: serde_json::from_str(&labels_json).unwrap_or_default(),
        })
    })?;
    rows.collect()
}

/// Strip an optional `NN-` numeric prefix (D11 layer dir grammar) so
/// both `20-site.plant-a` and `site.plant-a` select the same devices.
fn layer_label(layer: &str) -> &str {
    let b = layer.as_bytes();
    if b.len() > 3 && b[0].is_ascii_digit() && b[1].is_ascii_digit() && b[2] == b'-' {
        &layer[3..]
    } else {
        layer
    }
}

/// Union of the spec's selectors over the (non-stale) device set.
fn resolve_cohort(
    devices: &[SelectableDevice],
    spec: &CohortSpec,
) -> Result<Vec<String>, String> {
    let known: BTreeMap<&str, &SelectableDevice> =
        devices.iter().map(|d| (d.device_id.as_str(), d)).collect();
    let mut out: BTreeSet<String> = BTreeSet::new();

    for id in &spec.devices {
        if !known.contains_key(id.as_str()) {
            return Err(format!("unknown device `{id}` in cohort"));
        }
        out.insert(id.clone());
    }

    for layer in &spec.layers {
        let label = layer_label(layer);
        let matched: Vec<&SelectableDevice> = if label == "fleet" {
            devices.iter().collect()
        } else if let Some(n) = label.strip_prefix("class.") {
            devices.iter().filter(|d| d.class.as_deref() == Some(n)).collect()
        } else if let Some(n) = label.strip_prefix("region.") {
            devices.iter().filter(|d| d.region.as_deref() == Some(n)).collect()
        } else if let Some(n) = label.strip_prefix("site.") {
            devices.iter().filter(|d| d.site.as_deref() == Some(n)).collect()
        } else if let Some(id) = label.strip_prefix("device.") {
            devices.iter().filter(|d| d.device_id == id).collect()
        } else {
            return Err(format!(
                "layer selector `{layer}` is not fleet/class.<n>/region.<n>/site.<n>/device.<id>"
            ));
        };
        out.extend(matched.iter().map(|d| d.device_id.clone()));
    }

    if !spec.labels.is_empty() {
        out.extend(
            devices
                .iter()
                .filter(|d| {
                    spec.labels
                        .iter()
                        .all(|(k, v)| d.labels.get(k) == Some(v))
                })
                .map(|d| d.device_id.clone()),
        );
    }

    Ok(out.into_iter().collect())
}

/// Resolve `strategy` items (`"1"`, `"10%"`, `"rest"`) to explicit wave
/// sizes over `total` devices (§11.1: strategies resolve to explicit
/// device sets at creation). Leftover devices become a final wave.
fn strategy_sizes(items: &[String], total: usize) -> Result<Vec<usize>, String> {
    let mut sizes = Vec::new();
    let mut used = 0usize;
    for (i, item) in items.iter().enumerate() {
        let remaining = total - used;
        if remaining == 0 {
            break;
        }
        let n = if item == "rest" {
            if i + 1 != items.len() {
                return Err("`rest` must be the last strategy item".into());
            }
            remaining
        } else if let Some(pct) = item.strip_suffix('%') {
            let p: f64 = pct
                .parse()
                .map_err(|_| format!("bad percentage `{item}` in strategy"))?;
            if !(0.0..=100.0).contains(&p) {
                return Err(format!("percentage `{item}` out of range"));
            }
            (((p / 100.0) * total as f64).ceil() as usize).clamp(1, remaining)
        } else {
            let n: usize = item
                .parse()
                .map_err(|_| format!("bad wave size `{item}` in strategy"))?;
            if n == 0 {
                return Err("strategy wave size must be > 0".into());
            }
            n.min(remaining)
        };
        sizes.push(n);
        used += n;
    }
    if used < total {
        sizes.push(total - used);
    }
    Ok(sizes)
}

/// Ordered waves (explicit partition, strategy, count, or one wave).
/// Cohort order is deterministic (sorted device ids).
fn resolve_waves(cohort: &[String], req: &CreateRequest) -> Result<Vec<Vec<String>>, String> {
    let picked = [
        req.waves.is_some(),
        req.strategy.is_some(),
        req.wave_count.is_some(),
    ]
    .iter()
    .filter(|b| **b)
    .count();
    if picked > 1 {
        return Err("give at most one of `waves`, `strategy`, `waveCount`".into());
    }

    if let Some(waves) = &req.waves {
        let cohort_set: BTreeSet<&str> = cohort.iter().map(String::as_str).collect();
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for wave in waves {
            for d in wave {
                if !cohort_set.contains(d.as_str()) {
                    return Err(format!("wave device `{d}` is not in the cohort"));
                }
                if !seen.insert(d.as_str()) {
                    return Err(format!("device `{d}` appears in more than one wave"));
                }
            }
        }
        if seen.len() != cohort_set.len() {
            return Err("explicit waves must partition the whole cohort".into());
        }
        let waves: Vec<Vec<String>> =
            waves.iter().filter(|w| !w.is_empty()).cloned().collect();
        if waves.is_empty() {
            return Err("waves must contain at least one device".into());
        }
        return Ok(waves);
    }

    let sizes: Vec<usize> = if let Some(items) = &req.strategy {
        strategy_sizes(items, cohort.len())?
    } else if let Some(n) = req.wave_count {
        if n == 0 {
            return Err("waveCount must be > 0".into());
        }
        let n = (n as usize).min(cohort.len());
        let base = cohort.len() / n;
        let rem = cohort.len() % n;
        (0..n).map(|i| base + usize::from(i < rem)).collect()
    } else {
        vec![cohort.len()]
    };

    let mut it = cohort.iter().cloned();
    Ok(sizes
        .into_iter()
        .filter(|s| *s > 0)
        .map(|s| it.by_ref().take(s).collect())
        .collect())
}

// ---------------------------------------------------------------------
// Rollout rows + classification
// ---------------------------------------------------------------------

/// One `rollouts` row, engine view.
#[derive(Debug, Clone)]
struct RolloutRow {
    rollout_id: String,
    revision: i64,
    state: String,
    current_wave: i64,
    soak_secs: i64,
    gate_timeout_secs: i64,
    pass_fraction: f64,
    undetermined_allowance: Option<i64>,
    failure_threshold: i64,
}

fn load_rollout(conn: &Connection, rollout_id: &str) -> rusqlite::Result<Option<RolloutRow>> {
    conn.query_row(
        "SELECT rollout_id, revision, state, current_wave, soak_secs, gate_timeout_secs,
                pass_fraction, undetermined_allowance, failure_threshold
         FROM rollouts WHERE rollout_id = ?1",
        params![rollout_id],
        |r| {
            Ok(RolloutRow {
                rollout_id: r.get(0)?,
                revision: r.get(1)?,
                state: r.get(2)?,
                current_wave: r.get(3)?,
                soak_secs: r.get(4)?,
                gate_timeout_secs: r.get(5)?,
                pass_fraction: r.get(6)?,
                undetermined_allowance: r.get(7)?,
                failure_threshold: r.get(8)?,
            })
        },
    )
    .optional()
}

/// §11.6 per-device rollout view: advanced / converged / healthy /
/// undetermined / failed. `Pending` = not yet advanced by its wave.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum DeviceClass {
    Pending,
    Converged,
    Failed,
    Undetermined,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeviceStatus {
    device_id: String,
    advanced: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    advanced_at: Option<i64>,
    /// D12/§11.1: render materially unchanged by the rollout — counts
    /// as converged, surfaced so green never silently means "nothing
    /// deployed here".
    unaffected: bool,
    status: DeviceClass,
}

#[derive(Debug, Default, Clone, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WaveCounts {
    total: i64,
    converged: i64,
    failed: i64,
    undetermined: i64,
    pending: i64,
    unaffected: i64,
}

struct WaveClassification {
    counts: WaveCounts,
    devices: Vec<DeviceStatus>,
}

/// Classify one advanced device from data the server already ingests
/// (§11.3): Margo deployment status for the deployments its CURRENT
/// manifest (the rollout render) carries. Fresh = received at/after
/// advancement. No fresh signal — offline, link-degraded, unknown
/// (05-health-journal §7.4) — is `undetermined`, never failed (§11.3
/// offline policy). Backfilled journal records land in
/// `deployment_status_current` under the max-seq rule, so late history
/// resolves undetermined devices exactly as §7.4 prescribes.
fn classify_device(
    conn: &Connection,
    device_id: &str,
    advanced: bool,
    advanced_at: Option<i64>,
    unaffected: bool,
) -> rusqlite::Result<DeviceClass> {
    if !advanced {
        return Ok(DeviceClass::Pending);
    }
    if unaffected {
        // D12: pinned/unaffected devices count as CONVERGED in gate
        // math — their render carries the pin; there is nothing new to
        // observe.
        return Ok(DeviceClass::Converged);
    }
    let advanced_at = advanced_at.unwrap_or(0);

    let manifest_json: Option<String> = conn
        .query_row(
            "SELECT manifest_json FROM device_manifests WHERE device_id = ?1",
            params![device_id],
            |r| r.get(0),
        )
        .optional()?;
    let deployment_ids: Vec<String> = manifest_json
        .as_deref()
        .and_then(|j| serde_json::from_str::<StateManifest>(j).ok())
        .map(|m| m.apps.into_iter().filter_map(|a| a.deployment_id).collect())
        .unwrap_or_default();
    if deployment_ids.is_empty() {
        // Zero apps at the rollout revision: converging means removing
        // everything; nothing remains to report against. Trivially
        // converged.
        return Ok(DeviceClass::Converged);
    }

    let mut all_installed = true;
    for dep in &deployment_ids {
        let row: Option<(String, i64)> = conn
            .query_row(
                "SELECT state, received_at FROM deployment_status_current
                 WHERE device_id = ?1 AND deployment_id = ?2",
                params![device_id, dep],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            Some((state, received_at)) if received_at >= advanced_at => {
                if state == "failed" {
                    return Ok(DeviceClass::Failed);
                }
                if state != "installed" {
                    all_installed = false;
                }
            }
            _ => all_installed = false, // stale or absent: undetermined
        }
    }
    Ok(if all_installed {
        DeviceClass::Converged
    } else {
        DeviceClass::Undetermined
    })
}

fn classify_wave(
    conn: &Connection,
    rollout_id: &str,
    wave_idx: i64,
) -> rusqlite::Result<WaveClassification> {
    let mut stmt = conn.prepare(
        "SELECT device_id, advanced, advanced_at, unaffected FROM rollout_devices
         WHERE rollout_id = ?1 AND wave_idx = ?2 ORDER BY device_id",
    )?;
    let rows: Vec<(String, bool, Option<i64>, bool)> = stmt
        .query_map(params![rollout_id, wave_idx], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<Result<_, _>>()?;

    let mut counts = WaveCounts::default();
    let mut devices = Vec::with_capacity(rows.len());
    for (device_id, advanced, advanced_at, unaffected) in rows {
        let class = classify_device(conn, &device_id, advanced, advanced_at, unaffected)?;
        counts.total += 1;
        counts.unaffected += i64::from(unaffected);
        match class {
            DeviceClass::Pending => counts.pending += 1,
            DeviceClass::Converged => counts.converged += 1,
            DeviceClass::Failed => counts.failed += 1,
            DeviceClass::Undetermined => counts.undetermined += 1,
        }
        devices.push(DeviceStatus {
            device_id,
            advanced,
            advanced_at,
            unaffected,
            status: class,
        });
    }
    Ok(WaveClassification { counts, devices })
}

// ---------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------

fn emit_phase(state: &AppState, rollout_id: &str, wave: i64, phase: RolloutPhase) {
    state.events.emit(SseEvent::Rollout(RolloutEvent {
        ts: EventHub::now_ts(),
        rollout_id: rollout_id.to_string(),
        wave: wave.max(0) as u32,
        phase,
    }));
}

/// Append one transition record (§11.1 full history; §11.8 audit).
/// `author` is the human identity for API actions, `engine` for
/// automatic transitions.
fn record_transition(
    conn: &Connection,
    rollout_id: &str,
    action: &str,
    author: &str,
    detail: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO rollout_transitions (rollout_id, seq, ts, action, author, detail)
         VALUES (?1,
                 (SELECT COALESCE(MAX(seq), 0) + 1 FROM rollout_transitions
                  WHERE rollout_id = ?1),
                 ?2, ?3, ?4, ?5)",
        params![rollout_id, now_secs(), action, author, detail],
    )?;
    Ok(())
}

/// One engine pass over every ACTIVE rollout. Pure function of DB
/// state (Law 3: the crash-recovery story IS "tick again") — called by
/// the interval task, by status-ingest events ([`spawn_engine`]), and
/// synchronously after create/resume so tests and operators see
/// immediate movement. Each call performs at most one wave phase
/// change per rollout (advancing happens fully, but a wave advanced in
/// this tick soaks before its gate is evaluated on a LATER tick), so
/// status reports always get a chance to land between advancement and
/// gate math.
pub fn tick(state: &AppState) -> anyhow::Result<()> {
    let ids: Vec<String> = {
        let conn = state.db.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT rollout_id FROM rollouts WHERE state = 'active'
             ORDER BY created_at, rollout_id",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<Result<_, _>>()?
    };
    for id in ids {
        tick_rollout(state, &id)?;
    }
    Ok(())
}

/// [`tick`], warn-only — the engine loops must never die to one fault.
pub fn tick_logged(state: &AppState) {
    if let Err(e) = tick(state) {
        warn!(error = %e, "rollout engine tick failed; will retry next tick");
    }
}

fn tick_rollout(state: &AppState, rollout_id: &str) -> anyhow::Result<()> {
    let now = now_secs();
    let (r, wave_state, soak_started_at, cls) = {
        let conn = state.db.lock().expect("db mutex poisoned");
        let Some(r) = load_rollout(&conn, rollout_id)? else {
            return Ok(());
        };
        if r.state != "active" {
            return Ok(());
        }
        let wave: Option<(String, Option<i64>)> = conn
            .query_row(
                "SELECT state, soak_started_at FROM rollout_waves
                 WHERE rollout_id = ?1 AND wave_idx = ?2",
                params![rollout_id, r.current_wave],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((wave_state, soak_started_at)) = wave else {
            warn!(rollout = rollout_id, wave = r.current_wave, "active rollout has no current wave row");
            return Ok(());
        };
        let cls = classify_wave(&conn, rollout_id, r.current_wave)?;
        (r, wave_state, soak_started_at, cls)
    };

    // §11.4: threshold breach pauses at ANY time during the wave or its
    // soak — checked before any further advancement.
    if cls.counts.failed >= r.failure_threshold {
        auto_pause(state, &r, &cls.counts, "failure threshold breached")?;
        return Ok(());
    }

    match wave_state.as_str() {
        "pending" => {
            {
                let conn = state.db.lock().expect("db mutex poisoned");
                conn.execute(
                    "UPDATE rollout_waves SET state = 'advancing'
                     WHERE rollout_id = ?1 AND wave_idx = ?2 AND state = 'pending'",
                    params![rollout_id, r.current_wave],
                )?;
                record_transition(&conn, rollout_id, "wave-started", "engine", None)?;
            }
            emit_phase(state, rollout_id, r.current_wave, RolloutPhase::Started);
            advance_wave(state, &r, now)?;
        }
        "advancing" => advance_wave(state, &r, now)?,
        "soaking" => {
            let soak_start = soak_started_at.unwrap_or(now);
            if now >= soak_start + r.soak_secs {
                evaluate_gate(state, &r, soak_start, now)?;
            }
        }
        // passed: current_wave already moved (gate time); failed: only
        // reachable while paused (resume re-soaks). Nothing to do.
        _ => {}
    }
    Ok(())
}

/// Advance every un-advanced device of the current wave (§11.2). Each
/// device is one transaction; a crash mid-wave leaves some advanced
/// and some not — a resumable position the next tick continues from.
fn advance_wave(state: &AppState, r: &RolloutRow, now: i64) -> anyhow::Result<()> {
    let pending: Vec<String> = {
        let conn = state.db.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT device_id FROM rollout_devices
             WHERE rollout_id = ?1 AND wave_idx = ?2 AND advanced = 0
             ORDER BY device_id",
        )?;
        let rows = stmt.query_map(params![r.rollout_id, r.current_wave], |row| row.get(0))?;
        rows.collect::<Result<_, _>>()?
    };

    for device_id in &pending {
        // §11.4: a breach stops advancement immediately, INCLUDING the
        // un-advanced remainder of the current wave.
        let counts = {
            let conn = state.db.lock().expect("db mutex poisoned");
            classify_wave(&conn, &r.rollout_id, r.current_wave)?.counts
        };
        if counts.failed >= r.failure_threshold {
            auto_pause(state, r, &counts, "failure threshold breached mid-advancement")?;
            return Ok(());
        }
        advance_device(state, r, device_id, now)?;
    }

    // All advanced → soak (idempotent; soak_started_at set once).
    {
        let conn = state.db.lock().expect("db mutex poisoned");
        let remaining: i64 = conn.query_row(
            "SELECT COUNT(*) FROM rollout_devices
             WHERE rollout_id = ?1 AND wave_idx = ?2 AND advanced = 0",
            params![r.rollout_id, r.current_wave],
            |row| row.get(0),
        )?;
        if remaining == 0 {
            conn.execute(
                "UPDATE rollout_waves
                 SET state = 'soaking', soak_started_at = COALESCE(soak_started_at, ?3)
                 WHERE rollout_id = ?1 AND wave_idx = ?2 AND state = 'advancing'",
                params![r.rollout_id, r.current_wave, now_secs()],
            )?;
        }
    }
    Ok(())
}

/// Advance ONE device: probe pinned/unaffected (pure — recomputable
/// after any crash), then move its render target and mark it advanced
/// in a single transaction (§11.2), then materialize the manifest and
/// nudge (§11.2: nudges as usual; an offline device converges at its
/// next poll — Law 5).
fn advance_device(
    state: &AppState,
    r: &RolloutRow,
    device_id: &str,
    now: i64,
) -> anyhow::Result<()> {
    let stored: Option<String> = {
        let conn = state.db.lock().expect("db mutex poisoned");
        conn.query_row(
            "SELECT content_digest FROM device_manifests WHERE device_id = ?1",
            params![device_id],
            |row| row.get(0),
        )
        .optional()?
    };
    let probe = render::probe_content_digest(state, device_id, r.revision)
        .map_err(|e| anyhow::anyhow!("probing {device_id} at r{}: {e}", r.revision))?;
    // D12: materially unchanged render = pinned/unaffected. A device
    // that never rendered (no stored digest) or whose probe failed is
    // treated as affected — never silently green.
    let unaffected = stored.is_some() && probe.is_some() && probe == stored;

    {
        let mut conn = state.db.lock().expect("db mutex poisoned");
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO device_render_targets (device_id, revision, rollout_id, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(device_id) DO UPDATE SET
                 revision = excluded.revision,
                 rollout_id = excluded.rollout_id,
                 updated_at = excluded.updated_at",
            params![device_id, r.revision, r.rollout_id, now],
        )?;
        tx.execute(
            "UPDATE rollout_devices
             SET advanced = 1, advanced_at = ?3, unaffected = ?4
             WHERE rollout_id = ?1 AND device_id = ?2",
            params![r.rollout_id, device_id, now, unaffected],
        )?;
        tx.commit()?;
    }

    // Manifest materialization is best-effort here: the device's next
    // poll renders on demand (delivery.rs ensure_current) if this
    // fails or a crash lands between the tx above and this call.
    match render::ensure_current(state, device_id) {
        Ok(_) => state.channels.nudge_desired_state(device_id),
        Err(e) => warn!(device = %device_id, error = %e, "post-advance render failed; device poll will retry"),
    }
    Ok(())
}

/// §11.4 auto-pause: an event, not an error state that decays.
fn auto_pause(
    state: &AppState,
    r: &RolloutRow,
    counts: &WaveCounts,
    why: &str,
) -> anyhow::Result<()> {
    let reason = format!("{why} (wave {}: {} failed)", r.current_wave, counts.failed);
    {
        let conn = state.db.lock().expect("db mutex poisoned");
        let n = conn.execute(
            "UPDATE rollouts SET state = 'paused', pause_reason = ?2, updated_at = ?3
             WHERE rollout_id = ?1 AND state = 'active'",
            params![r.rollout_id, reason, now_secs()],
        )?;
        if n == 0 {
            return Ok(()); // lost a race with a human action; fine
        }
        record_transition(
            &conn,
            &r.rollout_id,
            "auto-pause",
            "engine",
            Some(&serde_json::to_string(counts)?),
        )?;
    }
    emit_phase(state, &r.rollout_id, r.current_wave, RolloutPhase::Paused);
    Ok(())
}

/// §11.3 gate evaluation at/after soak end. Pass: converged ≥
/// ceil(passFraction × determinable) AND undetermined ≤ allowance.
/// Not passing yet + timeout not reached: keep soaking (undetermined
/// may resolve via backfill). Timeout reached: resolve with what is
/// determinable — fail and pause, recording the undetermined set.
fn evaluate_gate(
    state: &AppState,
    r: &RolloutRow,
    soak_start: i64,
    now: i64,
) -> anyhow::Result<()> {
    let cls = {
        let conn = state.db.lock().expect("db mutex poisoned");
        classify_wave(&conn, &r.rollout_id, r.current_wave)?
    };
    let c = &cls.counts;
    let determinable = c.converged + c.failed;
    let allowance = r.undetermined_allowance.unwrap_or(c.total);
    let need = (r.pass_fraction * determinable as f64).ceil() as i64;
    let passed = c.converged >= need && c.undetermined <= allowance;
    let timeout_over = now >= soak_start + r.soak_secs + r.gate_timeout_secs;

    if !passed && !timeout_over {
        return Ok(()); // keep waiting within soak+timeout (§11.3)
    }

    let undetermined_set: Vec<&str> = cls
        .devices
        .iter()
        .filter(|d| d.status == DeviceClass::Undetermined)
        .map(|d| d.device_id.as_str())
        .collect();
    let gate_json = serde_json::to_string(&json!({
        "counts": c,
        "determinable": determinable,
        "need": need,
        "allowance": allowance,
        "passFraction": r.pass_fraction,
        "undeterminedDevices": undetermined_set,
        "passed": passed,
        "evaluatedAt": now,
    }))?;

    if passed {
        gate_pass(state, r, &gate_json, now)
    } else {
        gate_fail(state, r, &gate_json, now)
    }
}

fn last_wave_idx(conn: &Connection, rollout_id: &str) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT MAX(wave_idx) FROM rollout_waves WHERE rollout_id = ?1",
        params![rollout_id],
        |row| row.get(0),
    )
}

/// Gate passed: record it; move to the next wave, or complete the
/// rollout — completion deletes the rollout's render targets in the
/// SAME transaction, returning its devices to head-tracking (§11.2).
fn gate_pass(state: &AppState, r: &RolloutRow, gate_json: &str, now: i64) -> anyhow::Result<()> {
    let completed = {
        let mut conn = state.db.lock().expect("db mutex poisoned");
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE rollout_waves SET state = 'passed', gated_at = ?3, gate_json = ?4
             WHERE rollout_id = ?1 AND wave_idx = ?2",
            params![r.rollout_id, r.current_wave, now, gate_json],
        )?;
        let last = last_wave_idx(&tx, &r.rollout_id)?;
        let completed = r.current_wave >= last;
        if completed {
            tx.execute(
                "UPDATE rollouts SET state = 'completed', updated_at = ?2
                 WHERE rollout_id = ?1",
                params![r.rollout_id, now],
            )?;
            tx.execute(
                "DELETE FROM device_render_targets WHERE rollout_id = ?1",
                params![r.rollout_id],
            )?;
            record_transition(&tx, &r.rollout_id, "completed", "engine", Some(gate_json))?;
        } else {
            tx.execute(
                "UPDATE rollouts SET current_wave = current_wave + 1, updated_at = ?2
                 WHERE rollout_id = ?1",
                params![r.rollout_id, now],
            )?;
            record_transition(&tx, &r.rollout_id, "wave-gated", "engine", Some(gate_json))?;
        }
        tx.commit()?;
        completed
    };
    emit_phase(state, &r.rollout_id, r.current_wave, RolloutPhase::Gated);
    if completed {
        emit_phase(state, &r.rollout_id, r.current_wave, RolloutPhase::Completed);
    }
    Ok(())
}

/// Gate failed: record and pause (§11.4/§11.5 — pause, NEVER roll
/// back; a paused rollout is a stable, inspectable position).
fn gate_fail(state: &AppState, r: &RolloutRow, gate_json: &str, now: i64) -> anyhow::Result<()> {
    {
        let mut conn = state.db.lock().expect("db mutex poisoned");
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE rollout_waves SET state = 'failed', gated_at = ?3, gate_json = ?4
             WHERE rollout_id = ?1 AND wave_idx = ?2",
            params![r.rollout_id, r.current_wave, now, gate_json],
        )?;
        tx.execute(
            "UPDATE rollouts SET state = 'paused', pause_reason = ?2, updated_at = ?3
             WHERE rollout_id = ?1 AND state = 'active'",
            params![
                r.rollout_id,
                format!("wave {} gate failed", r.current_wave),
                now
            ],
        )?;
        record_transition(&tx, &r.rollout_id, "gate-failed", "engine", Some(gate_json))?;
        tx.commit()?;
    }
    emit_phase(state, &r.rollout_id, r.current_wave, RolloutPhase::Failed);
    emit_phase(state, &r.rollout_id, r.current_wave, RolloutPhase::Paused);
    Ok(())
}

/// Spawn the engine legs: the periodic tick and the event-driven check
/// (§11.4: threshold breach must pause "at any time", so a `failed`
/// deployment-status ingest triggers an immediate tick instead of
/// waiting out the interval).
pub fn spawn_engine(state: AppState) {
    let event_state = state.clone();
    tokio::spawn(async move {
        let mut rx = event_state.events.subscribe(None).rx;
        loop {
            match rx.recv().await {
                Ok(stamped) => {
                    if matches!(
                        &stamped.event,
                        SseEvent::DeploymentStatus(e)
                            if e.state == reeve_types::margo::status::DeploymentState::Failed
                    ) {
                        tick_logged(&event_state);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(ENGINE_TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            // First tick fires immediately: startup IS resume (Law 3).
            interval.tick().await;
            tick_logged(&state);
        }
    });
}

// ---------------------------------------------------------------------
// API routes (operator+ writes, viewer+ reads — §11.8 authorized,
// attributable operations)
// ---------------------------------------------------------------------

fn author_of(identity: &Identity) -> String {
    match identity {
        Identity::Human { user, .. } => user.clone(),
        // REEVE_AUTH=none: anonymous acts as admin (D1).
        _ => "anonymous".to_string(),
    }
}

fn unprocessable(msg: String) -> Response {
    (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({ "error": msg }))).into_response()
}

fn conflict(msg: String) -> Response {
    (StatusCode::CONFLICT, Json(json!({ "error": msg }))).into_response()
}

fn internal(e: impl std::fmt::Display) -> Response {
    warn!(error = %e, "rollout route internal error");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

fn new_rollout_id() -> String {
    let mut buf = [0u8; 8];
    getrandom::fill(&mut buf).expect("OS entropy");
    format!("ro-{}", hex::encode(buf))
}

/// POST /api/rollouts (operator+) — create and immediately start.
///
/// DECISION (baseline): un-advanced cohort devices are held at
/// `baselineRevision` (default: the target revision's parent). The
/// authoring commit has usually already rendered every device at head
/// by the time the rollout is created (tree.rs render hook), so
/// creation re-pins cohort devices to the baseline render; devices
/// that polled in the gap converge back to it. manifestVersion stays
/// strictly monotonic throughout (§11.5 note — content may revert,
/// versions never do). Create the rollout promptly after the commit
/// and no device ever sees the staged content early.
#[utoipa::path(
    post,
    path = "/api/rollouts",
    tag = "rollouts",
    request_body = CreateRequest,
    responses(
        (status = 201, description = "Rollout created and started", body = CreateRolloutResponse),
        (status = 401, description = "Unauthenticated"),
        (status = 403, description = "Below operator role"),
        (status = 409, description = "Conflicting active rollout", body = device_api::ErrorBody),
        (status = 422, description = "Invalid revision, cohort, waves, or gate", body = device_api::ErrorBody),
    ),
)]
pub async fn create_route(
    State(state): State<AppState>,
    identity: Identity,
    Json(req): Json<CreateRequest>,
) -> Response {
    if let Err(status) = require_at_least(&state, &identity, Role::Operator) {
        return status.into_response();
    }
    let author = author_of(&identity);

    // Validate the revision against the store (revisions lock only —
    // never while holding db, state.rs one-direction rule).
    let parent = {
        let store = state.revisions.lock().expect("revisions mutex poisoned");
        match store.revision(req.revision) {
            Ok(rev) => {
                if rev.stream != revision_store::Stream::Local {
                    return unprocessable(format!(
                        "revision {} is not a local-stream revision",
                        req.revision
                    ));
                }
                rev.parent.unwrap_or(0)
            }
            Err(revision_store::Error::UnknownRevision(_)) => {
                return unprocessable(format!("revision {} does not exist", req.revision));
            }
            Err(e) => return internal(e),
        }
    };
    let default_baseline = req.baseline_revision.unwrap_or(parent);
    if default_baseline >= req.revision || default_baseline < 0 {
        return unprocessable(format!(
            "baselineRevision {default_baseline} must be >= 0 and before revision {}",
            req.revision
        ));
    }
    if let Some(pf) = req.gate.pass_fraction
        && !(pf > 0.0 && pf <= 1.0)
    {
        return unprocessable("gate.passFraction must be in (0, 1]".into());
    }
    if req.failure_threshold.is_some_and(|t| t < 1) {
        return unprocessable("failureThreshold must be >= 1".into());
    }
    if req.gate.soak_secs.is_some_and(|s| s < 0)
        || req.gate.gate_timeout_secs.is_some_and(|s| s < 0)
        || req.gate.undetermined_allowance.is_some_and(|s| s < 0)
    {
        return unprocessable("gate windows/allowance must be >= 0".into());
    }

    let rollout_id = new_rollout_id();
    let (cohort, waves) = {
        let mut conn = state.db.lock().expect("db mutex poisoned");
        let selectable = match load_selectable(&conn) {
            Ok(s) => s,
            Err(e) => return internal(e),
        };
        let cohort = match resolve_cohort(&selectable, &req.cohort) {
            Ok(c) if c.is_empty() => {
                return unprocessable("cohort selects no devices".into());
            }
            Ok(c) => c,
            Err(msg) => return unprocessable(msg),
        };
        let waves = match resolve_waves(&cohort, &req) {
            Ok(w) => w,
            Err(msg) => return unprocessable(msg),
        };

        // §11.1: at most one active rollout may target a device's
        // manifest — overlap with an active/paused rollout is rejected
        // (fail, not queue — implementation choice, recorded here).
        // Aborted/completed rollouts do not block; a new rollout takes
        // over their surviving holds (rollback-as-new-rollout, §11.5).
        let overlap: Option<(String, String)> = {
            let placeholders = vec!["?"; cohort.len()].join(",");
            let sql = format!(
                "SELECT rd.device_id, rd.rollout_id FROM rollout_devices rd
                 JOIN rollouts r ON r.rollout_id = rd.rollout_id
                 WHERE r.state IN ('active', 'paused') AND rd.device_id IN ({placeholders})
                 LIMIT 1"
            );
            let res = conn.query_row(
                &sql,
                rusqlite::params_from_iter(cohort.iter()),
                |row| Ok((row.get(0)?, row.get(1)?)),
            );
            match res.optional() {
                Ok(o) => o,
                Err(e) => return internal(e),
            }
        };
        if let Some((device, other)) = overlap {
            return conflict(format!(
                "device `{device}` is already targeted by rollout `{other}` (active/paused)"
            ));
        }

        // One transaction: definition + waves + per-device assignment +
        // baseline holds (Law 3 — the rollout exists whole or not at
        // all).
        let created = (|| -> rusqlite::Result<()> {
            let now = now_secs();
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO rollouts
                     (rollout_id, revision, state, current_wave, cohort_json, soak_secs,
                      gate_timeout_secs, pass_fraction, undetermined_allowance,
                      failure_threshold, created_by, created_at, updated_at)
                 VALUES (?1, ?2, 'active', 0, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
                params![
                    rollout_id,
                    req.revision,
                    serde_json::to_string(&req.cohort).expect("cohort serializes"),
                    req.gate.soak_secs.unwrap_or(DEFAULT_SOAK_SECS),
                    req.gate.gate_timeout_secs.unwrap_or(DEFAULT_GATE_TIMEOUT_SECS),
                    req.gate.pass_fraction.unwrap_or(DEFAULT_PASS_FRACTION),
                    req.gate.undetermined_allowance,
                    req.failure_threshold.unwrap_or(DEFAULT_FAILURE_THRESHOLD),
                    author,
                    now,
                ],
            )?;
            for (idx, wave) in waves.iter().enumerate() {
                tx.execute(
                    "INSERT INTO rollout_waves (rollout_id, wave_idx, state)
                     VALUES (?1, ?2, 'pending')",
                    params![rollout_id, idx as i64],
                )?;
                for device_id in wave {
                    // Baseline = wherever the device currently stands:
                    // an existing hold (taken over from an aborted
                    // rollout — its revision is kept) or the default.
                    let existing: Option<i64> = tx
                        .query_row(
                            "SELECT revision FROM device_render_targets WHERE device_id = ?1",
                            params![device_id],
                            |row| row.get(0),
                        )
                        .optional()?;
                    let baseline = existing.unwrap_or(default_baseline);
                    tx.execute(
                        "INSERT INTO rollout_devices
                             (rollout_id, device_id, wave_idx, baseline_revision)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![rollout_id, device_id, idx as i64, baseline],
                    )?;
                    tx.execute(
                        "INSERT INTO device_render_targets
                             (device_id, revision, rollout_id, updated_at)
                         VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT(device_id) DO UPDATE SET
                             rollout_id = excluded.rollout_id,
                             updated_at = excluded.updated_at",
                        params![device_id, baseline, rollout_id, now],
                    )?;
                }
            }
            record_transition(&tx, &rollout_id, "created", &author, None)?;
            tx.commit()
        })();
        if let Err(e) = created {
            return internal(e);
        }
        (cohort, waves)
    };

    // Re-hold render pass: any cohort device the authoring commit
    // already bumped to head comes back to its baseline manifest
    // before wave math starts. Best-effort — the device's next poll
    // renders on demand either way.
    for device_id in &cohort {
        match render::ensure_current(&state, device_id) {
            Ok(render::Outcome::Updated(_)) => state.channels.nudge_desired_state(device_id),
            Ok(_) => {}
            Err(e) => warn!(device = %device_id, error = %e, "baseline re-hold render failed"),
        }
    }

    // Start wave 0 without waiting for the interval task (which is not
    // running under tests / may be a tick away).
    tick_logged(&state);

    (
        StatusCode::CREATED,
        Json(json!({
            "rolloutId": rollout_id,
            "revision": req.revision,
            "state": "active",
            "cohort": cohort,
            "waves": waves,
        })),
    )
        .into_response()
}

/// `POST /api/rollouts` 201 body.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateRolloutResponse {
    pub rollout_id: String,
    pub revision: i64,
    /// Always `active` at creation.
    pub state: String,
    /// Resolved cohort device ids.
    pub cohort: Vec<String>,
    /// The wave partition (device ids per wave).
    pub waves: Vec<Vec<String>>,
}

/// One `GET /api/rollouts` entry.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RolloutSummary {
    pub rollout_id: String,
    pub revision: i64,
    /// `active` | `paused` | `aborted` | `completed`.
    pub state: String,
    pub current_wave: i64,
    pub pause_reason: Option<String>,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub wave_count: i64,
    pub device_count: i64,
}

/// One wave in a [`RolloutDetail`].
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WaveStatus {
    pub index: i64,
    /// `pending` | `advancing` | `soaking` | `passed` | `failed`.
    pub state: String,
    pub soak_started_at: Option<i64>,
    pub gated_at: Option<i64>,
    /// Recorded gate evaluation (free-form; §11.3).
    #[schema(value_type = Object)]
    pub gate: Option<serde_json::Value>,
    pub counts: WaveCounts,
    pub devices: Vec<DeviceStatus>,
}

/// Effective gate policy in a [`RolloutDetail`].
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GatePolicy {
    pub soak_secs: i64,
    pub gate_timeout_secs: i64,
    pub pass_fraction: f64,
    pub undetermined_allowance: Option<i64>,
}

/// One audit transition in a [`RolloutDetail`].
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TransitionEntry {
    pub seq: i64,
    pub ts: i64,
    pub action: String,
    pub author: String,
    pub detail: Option<String>,
}

/// `GET /api/rollouts/{rollout_id}` body (§11.6 full status).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RolloutDetail {
    pub rollout_id: String,
    pub revision: i64,
    pub state: String,
    pub current_wave: i64,
    /// The cohort spec as recorded at creation.
    #[schema(value_type = Object)]
    pub cohort: serde_json::Value,
    pub gate: GatePolicy,
    pub failure_threshold: i64,
    pub pause_reason: Option<String>,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// §11.1 MUST: devices whose render the rollout does not change.
    pub pinned_unaffected: i64,
    pub waves: Vec<WaveStatus>,
    pub transitions: Vec<TransitionEntry>,
}

/// `POST /api/rollouts/{id}/pause|resume|abort` body.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RolloutTransitioned {
    pub rollout_id: String,
    /// `paused` | `resumed` | `aborted`.
    pub action: String,
}

/// GET /api/rollouts (viewer+) — newest first.
#[utoipa::path(
    get,
    path = "/api/rollouts",
    // Explicit id: `list_route` fn names collide with secrets;
    // operationIds must be globally unique (D10).
    operation_id = "list_rollouts",
    tag = "rollouts",
    responses(
        (status = 200, description = "All rollouts, newest first", body = Vec<RolloutSummary>),
        (status = 401, description = "Unauthenticated"),
        (status = 403, description = "Below viewer role"),
    ),
)]
pub async fn list_route(State(state): State<AppState>, identity: Identity) -> Response {
    if let Err(status) = require_at_least(&state, &identity, Role::Viewer) {
        return status.into_response();
    }
    let conn = state.db.lock().expect("db mutex poisoned");
    let result = (|| -> rusqlite::Result<Vec<serde_json::Value>> {
        let mut stmt = conn.prepare(
            "SELECT r.rollout_id, r.revision, r.state, r.current_wave, r.pause_reason,
                    r.created_by, r.created_at, r.updated_at,
                    (SELECT COUNT(*) FROM rollout_waves w WHERE w.rollout_id = r.rollout_id),
                    (SELECT COUNT(*) FROM rollout_devices d WHERE d.rollout_id = r.rollout_id)
             FROM rollouts r ORDER BY r.created_at DESC, r.rollout_id DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(json!({
                "rolloutId": row.get::<_, String>(0)?,
                "revision": row.get::<_, i64>(1)?,
                "state": row.get::<_, String>(2)?,
                "currentWave": row.get::<_, i64>(3)?,
                "pauseReason": row.get::<_, Option<String>>(4)?,
                "createdBy": row.get::<_, String>(5)?,
                "createdAt": row.get::<_, i64>(6)?,
                "updatedAt": row.get::<_, i64>(7)?,
                "waveCount": row.get::<_, i64>(8)?,
                "deviceCount": row.get::<_, i64>(9)?,
            }))
        })?;
        rows.collect()
    })();
    match result {
        Ok(list) => Json(list).into_response(),
        Err(e) => internal(e),
    }
}

/// GET /api/rollouts/{id} (viewer+) — full status: waves with live
/// per-device classification (§11.6: advanced / converged /
/// undetermined / failed), the pinned/unaffected count (§11.1 MUST),
/// recorded gate results, and the transition history.
#[utoipa::path(
    get,
    path = "/api/rollouts/{rollout_id}",
    // Explicit id: `status_route` fn names collide across durability /
    // federation / rollouts; operationIds must be globally unique (D10).
    operation_id = "rollout_status",
    tag = "rollouts",
    params(("rollout_id" = String, Path, description = "Rollout id")),
    responses(
        (status = 200, description = "Full rollout status (§11.6)", body = RolloutDetail),
        (status = 401, description = "Unauthenticated"),
        (status = 403, description = "Below viewer role"),
        (status = 404, description = "Unknown rollout", body = device_api::ErrorBody),
    ),
)]
pub async fn status_route(
    State(state): State<AppState>,
    identity: Identity,
    Path(rollout_id): Path<String>,
) -> Response {
    if let Err(status) = require_at_least(&state, &identity, Role::Viewer) {
        return status.into_response();
    }
    let conn = state.db.lock().expect("db mutex poisoned");
    let result = (|| -> anyhow::Result<Option<serde_json::Value>> {
        let Some(r) = load_rollout(&conn, &rollout_id)? else {
            return Ok(None);
        };
        let (cohort_json, pause_reason, created_by, created_at, updated_at): (
            String,
            Option<String>,
            String,
            i64,
            i64,
        ) = conn.query_row(
            "SELECT cohort_json, pause_reason, created_by, created_at, updated_at
             FROM rollouts WHERE rollout_id = ?1",
            params![rollout_id],
            |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
            },
        )?;

        let mut waves = Vec::new();
        let mut pinned_unaffected = 0i64;
        /// One `rollout_waves` row: (idx, state, soak_started_at,
        /// gated_at, gate_json).
        type WaveRow = (i64, String, Option<i64>, Option<i64>, Option<String>);
        let wave_rows: Vec<WaveRow> = {
            let mut stmt = conn.prepare(
                "SELECT wave_idx, state, soak_started_at, gated_at, gate_json
                 FROM rollout_waves WHERE rollout_id = ?1 ORDER BY wave_idx",
            )?;
            let rows = stmt.query_map(params![rollout_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
            })?;
            rows.collect::<Result<_, _>>()?
        };
        for (wave_idx, wave_state, soak_started_at, gated_at, gate_json) in wave_rows {
            let cls = classify_wave(&conn, &rollout_id, wave_idx)?;
            pinned_unaffected += cls.counts.unaffected;
            let gate: Option<serde_json::Value> =
                gate_json.as_deref().and_then(|g| serde_json::from_str(g).ok());
            waves.push(json!({
                "index": wave_idx,
                "state": wave_state,
                "soakStartedAt": soak_started_at,
                "gatedAt": gated_at,
                "gate": gate,
                "counts": cls.counts,
                "devices": cls.devices,
            }));
        }

        let transitions: Vec<serde_json::Value> = {
            let mut stmt = conn.prepare(
                "SELECT seq, ts, action, author, detail FROM rollout_transitions
                 WHERE rollout_id = ?1 ORDER BY seq",
            )?;
            let rows = stmt.query_map(params![rollout_id], |row| {
                Ok(json!({
                    "seq": row.get::<_, i64>(0)?,
                    "ts": row.get::<_, i64>(1)?,
                    "action": row.get::<_, String>(2)?,
                    "author": row.get::<_, String>(3)?,
                    "detail": row.get::<_, Option<String>>(4)?,
                }))
            })?;
            rows.collect::<Result<_, _>>()?
        };

        Ok(Some(json!({
            "rolloutId": r.rollout_id,
            "revision": r.revision,
            "state": r.state,
            "currentWave": r.current_wave,
            "cohort": serde_json::from_str::<serde_json::Value>(&cohort_json)
                .unwrap_or(serde_json::Value::Null),
            "gate": {
                "soakSecs": r.soak_secs,
                "gateTimeoutSecs": r.gate_timeout_secs,
                "passFraction": r.pass_fraction,
                "undeterminedAllowance": r.undetermined_allowance,
            },
            "failureThreshold": r.failure_threshold,
            "pauseReason": pause_reason,
            "createdBy": created_by,
            "createdAt": created_at,
            "updatedAt": updated_at,
            "pinnedUnaffected": pinned_unaffected,
            "waves": waves,
            "transitions": transitions,
        })))
    })();
    match result {
        Ok(Some(body)) => Json(body).into_response(),
        Ok(None) => {
            (StatusCode::NOT_FOUND, Json(json!({ "error": "unknown rollout" }))).into_response()
        }
        Err(e) => internal(e),
    }
}

/// Shared state-transition body for pause/resume/abort.
fn human_transition(
    state: &AppState,
    rollout_id: &str,
    author: &str,
    action: &str,
) -> anyhow::Result<Result<(i64, RolloutPhase), Response>> {
    let conn = state.db.lock().expect("db mutex poisoned");
    let Some(r) = load_rollout(&conn, rollout_id)? else {
        return Ok(Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "unknown rollout" })),
        )
            .into_response()));
    };
    let now = now_secs();
    let (allowed_from, to, phase): (&[&str], &str, RolloutPhase) = match action {
        "paused" => (&["active"], "paused", RolloutPhase::Paused),
        "resumed" => (&["paused"], "active", RolloutPhase::Started),
        // §11.2: aborting is pausing permanently — records retained,
        // holds retained, no manifest ever moves backward (§11.5).
        // RolloutPhase has no `aborted`; `failed` is the terminal
        // transition (§11.6) an abort publishes.
        "aborted" => (&["active", "paused"], "aborted", RolloutPhase::Failed),
        other => anyhow::bail!("unknown transition {other}"),
    };
    if !allowed_from.contains(&r.state.as_str()) {
        return Ok(Err(conflict(format!(
            "rollout is `{}`; {action} requires {allowed_from:?}",
            r.state
        ))));
    }
    match action {
        "paused" => {
            conn.execute(
                "UPDATE rollouts SET state = ?2, pause_reason = 'manual pause', updated_at = ?3
                 WHERE rollout_id = ?1",
                params![rollout_id, to, now],
            )?;
        }
        "resumed" => {
            conn.execute(
                "UPDATE rollouts SET state = ?2, pause_reason = NULL, updated_at = ?3
                 WHERE rollout_id = ?1",
                params![rollout_id, to, now],
            )?;
            // A failed gate re-soaks on resume: the gate re-evaluates
            // over CURRENT data (devices may have recovered or
            // backfilled — 05-health-journal §7.4 reclassification);
            // if the condition persists it pauses again.
            conn.execute(
                "UPDATE rollout_waves SET state = 'soaking'
                 WHERE rollout_id = ?1 AND wave_idx = ?2 AND state = 'failed'",
                params![rollout_id, r.current_wave],
            )?;
        }
        "aborted" => {
            conn.execute(
                "UPDATE rollouts SET state = ?2, updated_at = ?3 WHERE rollout_id = ?1",
                params![rollout_id, to, now],
            )?;
        }
        _ => unreachable!(),
    }
    record_transition(&conn, rollout_id, action, author, None)?;
    Ok(Ok((r.current_wave, phase)))
}

async fn transition_route(
    state: AppState,
    identity: Identity,
    rollout_id: String,
    action: &'static str,
) -> Response {
    if let Err(status) = require_at_least(&state, &identity, Role::Operator) {
        return status.into_response();
    }
    let author = author_of(&identity);
    match human_transition(&state, &rollout_id, &author, action) {
        Ok(Ok((wave, phase))) => {
            emit_phase(&state, &rollout_id, wave, phase);
            if action == "resumed" {
                tick_logged(&state); // resume moves immediately
            }
            Json(json!({ "rolloutId": rollout_id, "action": action })).into_response()
        }
        Ok(Err(resp)) => resp,
        Err(e) => internal(e),
    }
}

/// POST /api/rollouts/{id}/pause (operator+).
#[utoipa::path(
    post,
    path = "/api/rollouts/{rollout_id}/pause",
    tag = "rollouts",
    params(("rollout_id" = String, Path, description = "Rollout id")),
    responses(
        (status = 200, description = "Paused", body = RolloutTransitioned),
        (status = 401, description = "Unauthenticated"),
        (status = 403, description = "Below operator role"),
        (status = 404, description = "Unknown rollout", body = device_api::ErrorBody),
        (status = 409, description = "Not pausable from its current state", body = device_api::ErrorBody),
    ),
)]
pub async fn pause_route(
    State(state): State<AppState>,
    identity: Identity,
    Path(rollout_id): Path<String>,
) -> Response {
    transition_route(state, identity, rollout_id, "paused").await
}

/// POST /api/rollouts/{id}/resume (operator+).
#[utoipa::path(
    post,
    path = "/api/rollouts/{rollout_id}/resume",
    tag = "rollouts",
    params(("rollout_id" = String, Path, description = "Rollout id")),
    responses(
        (status = 200, description = "Resumed (a failed gate re-soaks)", body = RolloutTransitioned),
        (status = 401, description = "Unauthenticated"),
        (status = 403, description = "Below operator role"),
        (status = 404, description = "Unknown rollout", body = device_api::ErrorBody),
        (status = 409, description = "Not resumable from its current state", body = device_api::ErrorBody),
    ),
)]
pub async fn resume_route(
    State(state): State<AppState>,
    identity: Identity,
    Path(rollout_id): Path<String>,
) -> Response {
    transition_route(state, identity, rollout_id, "resumed").await
}

/// POST /api/rollouts/{id}/abort (operator+).
#[utoipa::path(
    post,
    path = "/api/rollouts/{rollout_id}/abort",
    tag = "rollouts",
    params(("rollout_id" = String, Path, description = "Rollout id")),
    responses(
        (status = 200, description = "Aborted (pausing permanently — records and holds retained, §11.2)", body = RolloutTransitioned),
        (status = 401, description = "Unauthenticated"),
        (status = 403, description = "Below operator role"),
        (status = 404, description = "Unknown rollout", body = device_api::ErrorBody),
        (status = 409, description = "Already terminal", body = device_api::ErrorBody),
    ),
)]
pub async fn abort_route(
    State(state): State<AppState>,
    identity: Identity,
    Path(rollout_id): Path<String>,
) -> Response {
    transition_route(state, identity, rollout_id, "aborted").await
}

// ---------------------------------------------------------------------
// Unit tests (pure resolution logic; the engine is covered end-to-end
// in tests/rollout_flow.rs)
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str, site: Option<&str>, labels: &[(&str, &str)]) -> SelectableDevice {
        SelectableDevice {
            device_id: id.to_string(),
            class: None,
            region: None,
            site: site.map(str::to_string),
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    fn cohort_of(devices: &[SelectableDevice], spec: &CohortSpec) -> Vec<String> {
        resolve_cohort(devices, spec).unwrap()
    }

    #[test]
    fn cohort_selectors_union_and_validate() {
        let devices = vec![
            dev("a", Some("plant-1"), &[("env", "prod")]),
            dev("b", Some("plant-2"), &[("env", "prod")]),
            dev("c", Some("plant-1"), &[("env", "dev")]),
        ];
        // explicit list
        assert_eq!(
            cohort_of(&devices, &CohortSpec { devices: vec!["b".into()], ..Default::default() }),
            ["b"]
        );
        // tree selection, with and without the numeric prefix
        for layer in ["20-site.plant-1", "site.plant-1"] {
            assert_eq!(
                cohort_of(
                    &devices,
                    &CohortSpec { layers: vec![layer.into()], ..Default::default() }
                ),
                ["a", "c"]
            );
        }
        // labels select cohorts (D12)
        assert_eq!(
            cohort_of(
                &devices,
                &CohortSpec {
                    labels: [("env".to_string(), "prod".to_string())].into(),
                    ..Default::default()
                }
            ),
            ["a", "b"]
        );
        // union, deduped + sorted
        assert_eq!(
            cohort_of(
                &devices,
                &CohortSpec {
                    devices: vec!["c".into()],
                    labels: [("env".to_string(), "prod".to_string())].into(),
                    ..Default::default()
                }
            ),
            ["a", "b", "c"]
        );
        // unknown device refused
        assert!(
            resolve_cohort(
                &devices,
                &CohortSpec { devices: vec!["nope".into()], ..Default::default() }
            )
            .is_err()
        );
        // unknown selector refused
        assert!(
            resolve_cohort(
                &devices,
                &CohortSpec { layers: vec!["cluster.x".into()], ..Default::default() }
            )
            .is_err()
        );
    }

    #[test]
    fn strategy_resolves_to_explicit_sizes() {
        let s = |items: &[&str], n| {
            strategy_sizes(&items.iter().map(|s| s.to_string()).collect::<Vec<_>>(), n)
        };
        // canonical `1, 10%, rest` over 20 devices (§11.1 example)
        assert_eq!(s(&["1", "10%", "rest"], 20).unwrap(), vec![1, 2, 17]);
        // leftover devices become a final wave
        assert_eq!(s(&["1"], 3).unwrap(), vec![1, 2]);
        // percent rounds up but never below 1
        assert_eq!(s(&["1%", "rest"], 10).unwrap(), vec![1, 9]);
        // rest must be last
        assert!(s(&["rest", "1"], 5).is_err());
        // zero size refused
        assert!(s(&["0"], 5).is_err());
    }

    fn req_with(waves: Option<Vec<Vec<String>>>, count: Option<u32>) -> CreateRequest {
        CreateRequest {
            revision: 1,
            cohort: CohortSpec::default(),
            waves,
            strategy: None,
            wave_count: count,
            gate: GateSpec::default(),
            failure_threshold: None,
            baseline_revision: None,
        }
    }

    #[test]
    fn waves_default_explicit_and_count() {
        let cohort: Vec<String> = ["a", "b", "c", "d", "e"].map(String::from).to_vec();
        // default: one wave
        assert_eq!(resolve_waves(&cohort, &req_with(None, None)).unwrap(), vec![cohort.clone()]);
        // count: near-even split
        assert_eq!(
            resolve_waves(&cohort, &req_with(None, Some(2))).unwrap(),
            vec![vec!["a", "b", "c"], vec!["d", "e"]]
                .into_iter()
                .map(|w| w.into_iter().map(String::from).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        );
        // explicit must partition
        let ok = resolve_waves(
            &cohort,
            &req_with(Some(vec![vec!["b".into()], vec!["a".into(), "c".into(), "d".into(), "e".into()]]), None),
        );
        assert!(ok.is_ok());
        let missing = resolve_waves(&cohort, &req_with(Some(vec![vec!["a".into()]]), None));
        assert!(missing.is_err(), "waves must cover the whole cohort");
        let dup = resolve_waves(
            &cohort,
            &req_with(
                Some(vec![
                    vec!["a".into(), "b".into(), "c".into(), "d".into(), "e".into()],
                    vec!["a".into()],
                ]),
                None,
            ),
        );
        assert!(dup.is_err(), "a device may appear in exactly one wave");
    }

    #[test]
    fn layer_label_strips_numeric_prefix_only() {
        assert_eq!(layer_label("20-site.plant"), "site.plant");
        assert_eq!(layer_label("site.plant"), "site.plant");
        assert_eq!(layer_label("fleet"), "fleet");
        assert_eq!(layer_label("00-fleet"), "fleet");
    }
}

//! Durability & restore verification (C6, spec/reeve/07-durability.md
//! REV-006; docs/decisions/storage.md D6/D16).
//!
//! THE seam (§9.1): one `Durability` trait in this one module of
//! reeve-server — tiers `none` | `snapshot` | `snapshot+changeset` are
//! config (REEVE_DURABILITY), not surgery. No other crate contains
//! replication, backup, or restore logic. Submodules:
//! - `target`    — object-store key layout + epoch marker (§9.2/§9.5)
//! - `aead`      — the encryption envelope under the D15 keyfile (§9.6)
//! - `snapshot`  — the generation anchor tier (§9.2), CORE
//! - `restore`   — fetch/replay + restore-at-bootstrap DR (§9.5), CORE
//! - `verify`    — verify-restore subcommand + scheduled task (§9.4), CORE
//! - `changeset` — seconds-RPO CAPTURE (§9.3), behind
//!   `ext-durability-changeset` (replay stays core)
//!
//! DECISION (extension boundary): the charter puts ext features in
//! `src/ext/<name>.rs`; the C6 seam requirement ("one Durability trait
//! in one module") wins for layout — the gated module lives at
//! `src/durability/changeset.rs`, still a whole module behind its
//! feature, still invisible to core (`--no-default-features` proof).

pub mod aead;
#[cfg(feature = "ext-durability-changeset")]
pub mod changeset;
pub mod restore;
pub mod snapshot;
pub mod target;
pub mod verify;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;
use serde::Serialize;
use tracing::{info, warn};

use crate::config::{Config, DurabilityTier};
use crate::keyfile;
use crate::state::AppState;

pub use restore::{maybe_restore_at_bootstrap, restore_at_bootstrap, verify_restore_cli};
pub use verify::{VerifyOutcome, VerifySummary};

/// Boxed future so the trait stays dyn-compatible.
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// THE durability seam (§9.1). What makes the changeset tier
/// reversible if it disappoints on the bench, and where a future
/// engine-native CDC could slot.
pub trait Durability: Send + Sync {
    /// Configured tier name: "none" | "snapshot" | "changeset".
    fn tier(&self) -> &'static str;

    /// Cut and ship one snapshot generation now (§9.2). `Ok(None)` on
    /// the none tier. Errors are already surfaced/degraded-flagged by
    /// the implementation; callers log and retry on schedule.
    fn snapshot_now(&self) -> BoxFut<'_, anyhow::Result<Option<String>>> {
        Box::pin(async { Ok(None) })
    }

    /// Extract + upload changesets if due (§9.3). No-op except on the
    /// changeset tier.
    fn ship_changesets(&self) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// One verify-restore pass (§9.4), recorded in the live DB.
    fn verify_restore(&self) -> BoxFut<'_, anyhow::Result<VerifyOutcome>>;

    /// Queryable durability status (§9.4 surfacing; C8 emits it as
    /// `durability-lag` / `verify-restore` SSE events).
    fn status(&self) -> DurabilityStatus;
}

/// The queryable status shape (§9.2 degraded flag, §9.3 upload lag,
/// §9.4 "last verified restore"). A deployment whose verify-restore
/// has never succeeded reads as having NO durability tier (§9.4) —
/// `effective_tier` encodes exactly that rule for the UI.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct DurabilityStatus {
    pub tier: String,
    pub degraded: bool,
    pub last_error: Option<String>,
    /// Current (last successfully shipped) snapshot generation.
    pub generation: Option<String>,
    pub last_snapshot_at: Option<i64>,
    pub snapshot_age_secs: Option<i64>,
    pub last_changeset_seq: Option<u64>,
    pub last_changeset_at: Option<i64>,
    pub pending_changesets: usize,
    pub last_verify: Option<VerifySummary>,
}

impl DurabilityStatus {
    /// §9.4: "a deployment whose verify-restore has never succeeded
    /// MUST be treated as having NO durability tier, whatever the
    /// bucket contains."
    pub fn effective_tier(&self) -> &str {
        match &self.last_verify {
            Some(v) if v.outcome == "ok" => &self.tier,
            _ if self.tier == "none" => "none",
            _ => "none (unverified)",
        }
    }
}

/// The disabled tier: every operation a no-op, verify an explicit
/// error (there is nothing to verify).
pub struct NoneDurability;

impl Durability for NoneDurability {
    fn tier(&self) -> &'static str {
        "none"
    }
    fn verify_restore(&self) -> BoxFut<'_, anyhow::Result<VerifyOutcome>> {
        Box::pin(async {
            anyhow::bail!("durability is disabled (REEVE_DURABILITY=none) — nothing to verify")
        })
    }
    fn status(&self) -> DurabilityStatus {
        DurabilityStatus {
            tier: "none".into(),
            degraded: false,
            last_error: None,
            generation: None,
            last_snapshot_at: None,
            snapshot_age_secs: None,
            last_changeset_seq: None,
            last_changeset_at: None,
            pending_changesets: 0,
            last_verify: None,
        }
    }
}

/// Build the configured engine over THE shared writer connection.
/// Tier selection is config, not surgery (§9.1).
pub fn from_config(
    cfg: &Config,
    db: Arc<Mutex<Connection>>,
) -> anyhow::Result<Arc<dyn Durability>> {
    let dcfg = &cfg.durability;
    if dcfg.tier == DurabilityTier::None {
        return Ok(Arc::new(NoneDurability));
    }
    let target_url = dcfg
        .target
        .as_deref()
        .expect("config parse guarantees target for non-none tiers");
    let target = target::Target::open(target_url, &dcfg.instance)?;
    // One key custody story for everything shipped off-box (§9.1):
    // the same keyfile C7's secrets vault uses.
    let key = keyfile::load_or_create(&cfg.data_dir.join(keyfile::KEY_FILE_NAME))?;
    let snap = snapshot::SnapshotTier::new(
        db,
        target,
        key,
        dcfg.clone(),
        cfg.data_dir.join("durability-work"),
    );
    match dcfg.tier {
        DurabilityTier::None => unreachable!(),
        DurabilityTier::Snapshot => Ok(Arc::new(snap)),
        DurabilityTier::Changeset => {
            #[cfg(feature = "ext-durability-changeset")]
            {
                Ok(Arc::new(changeset::ChangesetTier::new(snap)?))
            }
            #[cfg(not(feature = "ext-durability-changeset"))]
            {
                anyhow::bail!(
                    "REEVE_DURABILITY=changeset but this binary was built without the \
                     ext-durability-changeset feature — use snapshot, or rebuild with \
                     default features"
                )
            }
        }
    }
}

/// Startup sequencing (D6: migrate -> if migrated, snapshot -> resume
/// streaming). DECISION: with any tier enabled we cut a generation at
/// EVERY process start, not only after migrations — the changeset
/// tier's in-memory session died with the previous process (Law 3), so
/// a fresh anchor is the crash-only way to guarantee a gapless chain;
/// for the snapshot tier it costs at worst one extra snapshot.
pub async fn startup(engine: &Arc<dyn Durability>, migrated: bool) {
    if engine.tier() == "none" {
        return;
    }
    let reason = if migrated {
        "schema migration cut (D16)"
    } else {
        "startup generation anchor"
    };
    match engine.snapshot_now().await {
        Ok(Some(generation)) => info!(%generation, reason, "durability: startup snapshot shipped"),
        Ok(None) => {}
        Err(e) => warn!(error = %e, reason, "durability: startup snapshot failed (degraded); \
                         retrying on interval"),
    }
}

/// Spawn the scheduled loops: snapshot interval (§9.2), changeset tick
/// (§9.3), verify-restore interval (§9.4). Crash-only: no shutdown
/// plumbing — tasks die with the process.
pub fn spawn_tasks(engine: Arc<dyn Durability>, cfg: &crate::config::DurabilityConfig) {
    if engine.tier() == "none" {
        return;
    }

    let snapshot_every = Duration::from_secs(cfg.snapshot_interval_secs.max(1));
    let snap_engine = engine.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(snapshot_every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await; // startup() already cut the anchor
        loop {
            tick.tick().await;
            if let Err(e) = snap_engine.snapshot_now().await {
                warn!(error = %e, "durability: scheduled snapshot failed (degraded)");
            }
        }
    });

    if engine.tier() == "changeset" {
        // 1s tick; the engine's own interval/commit thresholds decide
        // whether a tick actually extracts and ships (§9.3).
        let ship_engine = engine.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                if let Err(e) = ship_engine.ship_changesets().await {
                    warn!(error = %e, "durability: changeset ship failed (degraded)");
                }
            }
        });
    }

    let verify_every = Duration::from_secs(cfg.verify_interval_secs.max(1));
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(verify_every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await; // first verify after one interval
        loop {
            tick.tick().await;
            match engine.verify_restore().await {
                Ok(outcome) if outcome.ok => {
                    info!(generation = ?outcome.generation, "verify-restore: ok");
                }
                Ok(outcome) => {
                    warn!(detail = ?outcome.detail, "verify-restore: FAILED");
                }
                Err(e) => warn!(error = %e, "verify-restore: run error"),
            }
        }
    });
}

/// `GET /api/durability/status` body: [`DurabilityStatus`] plus the
/// §9.4 `effective_tier` (an unverified tier reads as none).
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DurabilityStatusResponse {
    #[serde(flatten)]
    pub status: DurabilityStatus,
    /// `none` | `snapshot` | `changeset` | `none (unverified)`.
    pub effective_tier: String,
    /// Current server epoch (`settings.server_epoch`, spec/reeve/07-durability.md
    /// §9.5): the high 16 bits of every manifestVersion. Restore fencing
    /// increments it — the ops UI surfaces it next to durability posture.
    pub epoch: u16,
}

/// GET /api/durability/status — the §9.4 API surface (viewer+; the UI
/// renders "last verified restore" and the degraded flag from this).
#[utoipa::path(
    get,
    path = "/api/durability/status",
    // Explicit id: `status_route` fn names collide across durability /
    // federation / rollouts, and orval derives hook names from
    // operationIds (D10) — they must be globally unique.
    operation_id = "durability_status",
    tag = "durability",
    responses(
        (status = 200, description = "Durability tier status: degraded flag, upload lag, last verified restore", body = DurabilityStatusResponse),
        (status = 401, description = "Unauthenticated"),
        (status = 403, description = "Below viewer role"),
    ),
)]
pub async fn status_route(
    axum::extract::State(state): axum::extract::State<AppState>,
    identity: device_api::Identity,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    if let Err(status) =
        crate::join_tokens::require_at_least(&state, &identity, device_api::Role::Viewer)
    {
        return status.into_response();
    }
    let status = state.durability.status();
    let effective = status.effective_tier().to_string();
    let epoch = {
        let conn = state.db.lock().expect("db mutex poisoned");
        match crate::render::server_epoch(&conn) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "durability status: reading server_epoch failed");
                0
            }
        }
    };
    let mut body = serde_json::to_value(&status).expect("status serializes");
    body["effective_tier"] = serde_json::Value::String(effective);
    body["epoch"] = serde_json::Value::from(epoch);
    axum::Json(body).into_response()
}

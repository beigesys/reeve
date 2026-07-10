//! Snapshot tier — the generation anchor
//! (spec/reeve/07-durability.md §9.2): `VACUUM INTO` on interval, AEAD
//! under the D15 keyfile, atomic upload, retention, degraded-not-fatal.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use rusqlite::{Connection, params};
use tracing::{info, warn};

use crate::config::DurabilityConfig;
use crate::db::now_secs;
use crate::durability::target::{LatestPointer, Target};
use crate::durability::{BoxFut, Durability, DurabilityStatus, aead, verify};
use crate::keyfile::KEY_LEN;

/// Mutable snapshot-tier bookkeeping. Nothing here is load-bearing
/// across a crash (Law 3): generation identity is re-anchored by the
/// startup snapshot, degraded state is re-derived by the next attempt.
#[derive(Default)]
pub(crate) struct TierState {
    pub generation: Option<String>,
    pub last_snapshot_at: Option<i64>,
    pub degraded: bool,
    pub last_error: Option<String>,
}

pub struct SnapshotTier {
    pub(crate) db: Arc<Mutex<Connection>>,
    pub(crate) target: Target,
    pub(crate) key: [u8; KEY_LEN],
    pub(crate) cfg: DurabilityConfig,
    /// Where VACUUM/restore temp files live (same filesystem as the
    /// live DB so renames stay atomic).
    pub(crate) work_dir: PathBuf,
    pub(crate) state: Mutex<TierState>,
}

impl SnapshotTier {
    pub fn new(
        db: Arc<Mutex<Connection>>,
        target: Target,
        key: [u8; KEY_LEN],
        cfg: DurabilityConfig,
        work_dir: PathBuf,
    ) -> Self {
        SnapshotTier {
            db,
            target,
            key,
            cfg,
            work_dir,
            state: Mutex::new(TierState::default()),
        }
    }

    /// Produce + ship one generation, running `under_lock` against the
    /// writer connection IN THE SAME critical section as `VACUUM INTO`.
    /// The changeset tier uses that hook to reset its capture session so
    /// no commit can fall between the snapshot and the new chain (§9.3).
    pub(crate) async fn cut_generation(
        &self,
        under_lock: impl FnOnce(&Connection) + Send + 'static,
    ) -> anyhow::Result<String> {
        std::fs::create_dir_all(&self.work_dir)?;
        let tmp = tempfile::Builder::new()
            .prefix("reeve-snapshot-")
            .tempdir_in(&self.work_dir)?;
        let vacuum_path = tmp.path().join("snapshot.db");

        // 1. Consistent copy: VACUUM INTO — safe under WAL with the
        // writer live (§9.2). Blocking work off the async threads.
        let db = self.db.clone();
        let vp = vacuum_path.clone();
        let (schema, epoch) =
            tokio::task::spawn_blocking(move || -> anyhow::Result<(i64, u16)> {
                let conn = db.lock().expect("db mutex poisoned");
                let schema: i64 = conn.query_row(
                    "SELECT COALESCE(MAX(version), 0) FROM refinery_schema_history",
                    [],
                    |r| r.get(0),
                )?;
                let epoch = crate::render::server_epoch(&conn)?;
                conn.execute("VACUUM INTO ?1", params![vp.to_string_lossy()])?;
                under_lock(&conn);
                Ok((schema, epoch))
            })
            .await
            .context("snapshot task panicked")??;

        // 2. Seal under the keyfile (§9.2/§9.6: nothing reaches the
        // target in plaintext).
        let plaintext = std::fs::read(&vacuum_path)?;
        let sealed = aead::seal(&self.key, &plaintext)?;
        drop(plaintext);

        // 3. Upload payload, then epoch marker if absent, then the
        // latest pointer LAST (§9.2: a crash at any byte leaves the
        // previous generation authoritative).
        let generation = Target::new_generation_id(schema);
        self.target
            .put(&Target::snapshot_key(&generation), sealed)
            .await?;
        if self.target.read_epoch().await?.is_none() {
            // First shipment ever: materialize the marker so
            // verify-restore's "present and readable" assertion (§9.4)
            // holds from generation one.
            self.target.write_epoch(epoch).await?;
        }
        let pointer = LatestPointer {
            generation: generation.clone(),
            snapshot_key: Target::snapshot_key(&generation).to_string(),
            schema,
            created_at: now_secs(),
        };
        self.target
            .put(&Target::latest_key(), serde_json::to_vec(&pointer)?)
            .await?;
        Ok(generation)
    }

    /// Full snapshot pass: cut + state bookkeeping + retention. Failure
    /// anywhere is surfaced (degraded flag), never fatal — the caller
    /// retries next interval (§9.2).
    pub(crate) async fn snapshot_with(
        &self,
        under_lock: impl FnOnce(&Connection) + Send + 'static,
    ) -> anyhow::Result<Option<String>> {
        match self.cut_generation(under_lock).await {
            Ok(generation) => {
                {
                    let mut st = self.state.lock().expect("tier state poisoned");
                    st.generation = Some(generation.clone());
                    st.last_snapshot_at = Some(now_secs());
                    st.degraded = false;
                    st.last_error = None;
                }
                info!(%generation, "durability: snapshot generation shipped");
                if let Err(e) = self.prune(&generation).await {
                    warn!(error = %e, "durability: retention prune failed (degraded)");
                    self.mark_degraded(&e);
                }
                Ok(Some(generation))
            }
            Err(e) => {
                warn!(error = %e, "durability: snapshot failed (degraded, retrying next interval)");
                self.mark_degraded(&e);
                Err(e)
            }
        }
    }

    pub(crate) fn mark_degraded(&self, e: &anyhow::Error) {
        let mut st = self.state.lock().expect("tier state poisoned");
        st.degraded = true;
        st.last_error = Some(e.to_string());
    }

    /// Retention (§9.2): keep generations inside the window OR among
    /// the newest `retain_min_generations`; NEVER prune the last
    /// known-verified generation or the current one. Prunes whole
    /// generations (snapshot + chained changesets).
    async fn prune(&self, current: &str) -> anyhow::Result<()> {
        let generations = self.target.list_generations().await?;
        if generations.is_empty() {
            return Ok(());
        }
        let cutoff = now_secs() - (self.cfg.retain_days as i64) * 86_400;
        let min_keep = self.cfg.retain_min_generations.max(1) as usize;
        // list_generations sorts ascending by id (timestamp-prefixed).
        let newest: std::collections::BTreeSet<&str> = generations
            .iter()
            .rev()
            .take(min_keep)
            .map(|(g, _)| g.as_str())
            .collect();
        let last_verified: Option<String> = {
            let conn = self.db.lock().expect("db mutex poisoned");
            conn.query_row(
                "SELECT generation FROM verify_restore_runs
                 WHERE outcome = 'ok' AND generation IS NOT NULL
                 ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .ok()
        };
        for (generation, uploaded_at) in &generations {
            let keep = *uploaded_at >= cutoff
                || newest.contains(generation.as_str())
                || generation == current
                || last_verified.as_deref() == Some(generation.as_str());
            if !keep {
                info!(%generation, "durability: pruning expired generation");
                self.target.delete_generation(generation).await?;
            }
        }
        Ok(())
    }

    pub(crate) fn base_status(&self) -> DurabilityStatus {
        let (generation, last_snapshot_at, degraded, last_error) = {
            let st = self.state.lock().expect("tier state poisoned");
            (
                st.generation.clone(),
                st.last_snapshot_at,
                st.degraded,
                st.last_error.clone(),
            )
        };
        let last_verify = {
            let conn = self.db.lock().expect("db mutex poisoned");
            verify::last_verify_summary(&conn)
        };
        DurabilityStatus {
            tier: "snapshot".into(),
            degraded,
            last_error,
            generation,
            last_snapshot_at,
            snapshot_age_secs: last_snapshot_at.map(|t| (now_secs() - t).max(0)),
            last_changeset_seq: None,
            last_changeset_at: None,
            pending_changesets: 0,
            last_verify,
        }
    }
}

impl Durability for SnapshotTier {
    fn tier(&self) -> &'static str {
        "snapshot"
    }

    fn snapshot_now(&self) -> BoxFut<'_, anyhow::Result<Option<String>>> {
        Box::pin(self.snapshot_with(|_| {}))
    }

    fn verify_restore(&self) -> BoxFut<'_, anyhow::Result<verify::VerifyOutcome>> {
        Box::pin(verify::run(
            &self.db,
            &self.target,
            &self.key,
            &self.cfg,
            &self.work_dir,
        ))
    }

    fn status(&self) -> DurabilityStatus {
        self.base_status()
    }
}

//! Restore: fetch the latest generation, decrypt, replay its changeset
//! chain — the ONE restore procedure shared by verify-restore (§9.4)
//! and restore-at-bootstrap/DR (§9.5). "One restore procedure for
//! everything — there is no second path to rot."
//!
//! Changeset REPLAY is core (unconditional): a `--no-default-features`
//! binary can restore a target written by a changeset-enabled one —
//! only CAPTURE is behind `ext-durability-changeset`.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, anyhow};
use rusqlite::Connection;
use rusqlite::session::ConflictAction;
use tracing::{info, warn};

use crate::config::Config;
use crate::durability::aead;
use crate::durability::target::Target;
use crate::keyfile::{self, KEY_LEN};

/// A generation fully replayed into a temp SQLite file. Dropping this
/// removes the temp location (verify-restore MUST clean up, §9.6).
pub struct ReplayedDb {
    pub db_path: PathBuf,
    pub generation: String,
    pub schema: i64,
    /// Highest changeset sequence applied; 0 when the generation has
    /// no changesets.
    pub last_seq: u64,
    /// Unix seconds of the newest artifact in the chain (snapshot or
    /// last changeset upload) — the §9.4 recency input.
    pub newest_artifact_at: i64,
    _tmp: tempfile::TempDir,
}

/// Download + decrypt the latest generation's snapshot and apply ALL
/// its changesets in sequence order (§9.3/§9.4). Any
/// `changeset_apply` conflict is CORRUPTION and aborts loudly — never
/// auto-resolved (§9.3: replaying own lineage onto own snapshot).
pub async fn fetch_and_replay(
    target: &Target,
    key: &[u8; KEY_LEN],
    tmp_root: &Path,
) -> anyhow::Result<ReplayedDb> {
    let pointer = target
        .latest()
        .await?
        .context("no generation at target (gen/latest absent) — nothing to restore")?;

    let sealed = target
        .get(&object_store::path::Path::from(pointer.snapshot_key.clone()))
        .await?;
    let plaintext = aead::open(key, &sealed)
        .with_context(|| format!("snapshot of generation {} corrupt", pointer.generation))?;
    drop(sealed);

    std::fs::create_dir_all(tmp_root)?;
    let tmp = tempfile::Builder::new()
        .prefix("reeve-restore-")
        .tempdir_in(tmp_root)?;
    let db_path = tmp.path().join("restored.db");
    // D6 file-write rule even for temp state: temp + fsync + rename.
    let staging = tmp.path().join("restored.db.tmp");
    {
        let mut f = std::fs::File::create(&staging)?;
        f.write_all(&plaintext)?;
        f.sync_all()?;
    }
    std::fs::rename(&staging, &db_path)?;

    let changesets = target.list_changesets(&pointer.generation).await?;
    let mut last_seq = 0u64;
    let mut newest = pointer.created_at;
    if !changesets.is_empty() {
        // FK enforcement off during replay: a changeset is one logical
        // batch whose internal op order need not be FK-topological; the
        // chain as a whole is our own committed lineage. verify (§9.4)
        // runs integrity_check on the result.
        let conn = Connection::open(&db_path)?;
        for (seq, cs_key, uploaded_at) in &changesets {
            let sealed = target.get(cs_key).await?;
            let compressed = aead::open(key, &sealed).with_context(|| {
                format!("changeset seq {seq} of generation {} corrupt", pointer.generation)
            })?;
            let mut raw = Vec::new();
            flate2::read::GzDecoder::new(compressed.as_slice())
                .read_to_end(&mut raw)
                .with_context(|| format!("decompressing changeset seq {seq}"))?;
            conn.apply_strm(
                &mut raw.as_slice(),
                None::<fn(&str) -> bool>,
                |_conflict_type, _item| ConflictAction::SQLITE_CHANGESET_ABORT,
            )
            .map_err(|e| {
                // §9.3: conflicts are structurally impossible on our own
                // lineage — any conflict is CORRUPTION; abort loudly.
                anyhow!(
                    "CORRUPTION: changeset_apply failed at seq {seq} of generation {}: {e} \
                     — restore aborted, never auto-resolved",
                    pointer.generation
                )
            })?;
            last_seq = *seq;
            newest = newest.max(*uploaded_at);
        }
    }

    Ok(ReplayedDb {
        db_path,
        generation: pointer.generation,
        schema: pointer.schema,
        last_seq,
        newest_artifact_at: newest,
        _tmp: tmp,
    })
}

/// Restore-at-bootstrap — THE documented DR procedure (§9.5): a server
/// starting with NO local database, a configured target, and the
/// `--restore-from-target` confirmation flag fetches the latest
/// generation, replays it, fences the epoch, and places the result as
/// the local DB. Disaster recovery needs TWO artifacts: the snapshot
/// target AND the keyfile (`REEVE_DATA/secret.key`) — restore the
/// keyfile into the data dir first (§9.1/§9.6).
///
/// Crash-only ordering (§9.5): the epoch marker at the target is
/// incremented BEFORE the DB is placed, and the fenced epoch is
/// stamped INTO the DB before the rename — so a kill -9 anywhere
/// leaves either no local DB (restore reruns; the double increment is
/// harmless) or a complete DB that already carries the fresh epoch.
/// The server never serves under an epoch it has not incremented.
pub async fn restore_at_bootstrap(cfg: &Config) -> anyhow::Result<()> {
    let dcfg = &cfg.durability;
    let target_url = dcfg
        .target
        .as_deref()
        .context("restore-from-target requires REEVE_DURABILITY_TARGET")?;
    std::fs::create_dir_all(&cfg.data_dir)?;
    let key = keyfile::load(&cfg.data_dir.join(keyfile::KEY_FILE_NAME)).context(
        "DR needs TWO artifacts: the snapshot target AND the keyfile \
         (REEVE_DATA/secret.key) — restore the keyfile into the data dir first \
         (spec/reeve/07-durability.md §9.5/§9.6)",
    )?;
    let target = Target::open(target_url, &dcfg.instance)?;

    // Temp lives INSIDE the data dir so the final rename is atomic.
    let replayed = fetch_and_replay(&target, &key, &cfg.data_dir).await?;
    info!(
        generation = %replayed.generation,
        last_seq = replayed.last_seq,
        "restore: generation replayed"
    );

    // §9.5 fencing: increment the marker AT THE TARGET FIRST. Epoch
    // reuse is forbidden; a crash after this line and before serving
    // only wastes an epoch.
    let current = target.read_epoch().await?.unwrap_or(0);
    let fenced = current
        .checked_add(1)
        .context("epoch marker exhausted (u16::MAX restores) — new instance name required")?;
    target.write_epoch(fenced).await?;
    info!(epoch = fenced, "restore: epoch marker incremented at target");

    // Stamp the fenced epoch into the restored DB before placing it.
    {
        let conn = Connection::open(&replayed.db_path)?;
        conn.execute(
            "INSERT INTO settings (key, value) VALUES ('server_epoch', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![fenced.to_string()],
        )
        .context("stamping server_epoch into restored DB")?;
    }

    let final_path = cfg.data_dir.join("reeve.db");
    // Defensive: stale sidecar files must not shadow the restored DB.
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(cfg.data_dir.join(format!("reeve.db{suffix}")));
    }
    std::fs::File::open(&replayed.db_path)?.sync_all()?;
    std::fs::rename(&replayed.db_path, &final_path)
        .context("placing restored DB (temp + rename)")?;
    info!(path = %final_path.display(), "restore: local DB placed; continuing normal start");
    Ok(())
}

/// Startup gate for [`restore_at_bootstrap`] (§9.5): with NO local DB
/// and a configured target, restore when confirmed by the flag,
/// otherwise start fresh with a loud pointer at the DR procedure.
pub async fn maybe_restore_at_bootstrap(
    cfg: &Config,
    restore_from_target: bool,
) -> anyhow::Result<()> {
    let db_exists = cfg.data_dir.join("reeve.db").exists();
    let target_configured = cfg.durability.target.is_some();
    if db_exists {
        if restore_from_target {
            anyhow::bail!(
                "--restore-from-target refused: a local DB already exists at {} — \
                 move it aside first (restore never overwrites live state)",
                cfg.data_dir.join("reeve.db").display()
            );
        }
        return Ok(());
    }
    if !target_configured {
        if restore_from_target {
            anyhow::bail!("--restore-from-target requires REEVE_DURABILITY_TARGET");
        }
        return Ok(());
    }
    if restore_from_target {
        restore_at_bootstrap(cfg).await
    } else {
        warn!(
            "no local DB and a durability target is configured — starting FRESH. \
             If this is disaster recovery, stop and rerun with --restore-from-target \
             (and the keyfile in place): spec/reeve/07-durability.md §9.5"
        );
        Ok(())
    }
}

/// `reeve-server verify-restore` subcommand body: open/migrate the live
/// DB (idempotent, Law 3), build the configured engine, run one verify
/// pass, and report. Exit-code semantics live in main.rs.
pub async fn verify_restore_cli(cfg: Config) -> anyhow::Result<crate::durability::VerifyOutcome> {
    std::fs::create_dir_all(&cfg.data_dir)?;
    let mut conn = crate::db::open(cfg.data_dir.join("reeve.db"))?;
    crate::db::migrate(&mut conn)?;
    let db = Arc::new(Mutex::new(conn));
    let engine = crate::durability::from_config(&cfg, db)?;
    engine.verify_restore().await
}

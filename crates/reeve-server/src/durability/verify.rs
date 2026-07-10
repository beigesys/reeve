//! verify-restore (spec/reeve/07-durability.md §9.4, MUST): prove the
//! WHOLE chain — download, decrypt, replay, open, integrity-check,
//! schema known, recency, epoch marker readable — and record the
//! result in the live DB. Runs as the `reeve-server verify-restore`
//! subcommand AND as a scheduled internal task. Replays read-only in a
//! temp location and cleans up (§9.6) — never against the live DB.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension as _, params};
use serde::Serialize;

use crate::config::DurabilityConfig;
use crate::db::now_secs;
use crate::durability::restore;
use crate::durability::target::Target;
use crate::keyfile::KEY_LEN;

/// Result of one verify-restore run — also the recorded row shape.
#[derive(Debug, Clone, Serialize)]
pub struct VerifyOutcome {
    pub ok: bool,
    pub generation: Option<String>,
    pub last_seq: Option<i64>,
    pub started_at: i64,
    pub finished_at: i64,
    /// Failure detail when `ok` is false.
    pub detail: Option<String>,
}

/// Latest recorded run, for the status surface ("last verified
/// restore: <when>", §9.4).
#[derive(Debug, Clone, Serialize)]
pub struct VerifySummary {
    pub finished_at: i64,
    pub outcome: String,
    pub generation: Option<String>,
    pub last_seq: Option<i64>,
    pub detail: Option<String>,
}

pub(crate) fn last_verify_summary(conn: &Connection) -> Option<VerifySummary> {
    conn.query_row(
        "SELECT finished_at, outcome, generation, last_seq, detail
         FROM verify_restore_runs ORDER BY id DESC LIMIT 1",
        [],
        |r| {
            Ok(VerifySummary {
                finished_at: r.get(0)?,
                outcome: r.get(1)?,
                generation: r.get(2)?,
                last_seq: r.get(3)?,
                detail: r.get(4)?,
            })
        },
    )
    .optional()
    .ok()
    .flatten()
}

/// One full verify pass. The run itself never errors for chain
/// problems — those become a recorded `failed` row and `ok: false`;
/// only recording infrastructure faults surface as `Err`.
pub(crate) async fn run(
    live_db: &Arc<Mutex<Connection>>,
    target: &Target,
    key: &[u8; KEY_LEN],
    cfg: &DurabilityConfig,
    work_dir: &Path,
) -> anyhow::Result<VerifyOutcome> {
    let started_at = now_secs();
    let chain = check_chain(target, key, cfg, work_dir).await;
    let finished_at = now_secs();

    let outcome = match chain {
        Ok((generation, last_seq)) => VerifyOutcome {
            ok: true,
            generation: Some(generation),
            last_seq: Some(last_seq as i64),
            started_at,
            finished_at,
            detail: None,
        },
        Err(e) => VerifyOutcome {
            ok: false,
            generation: None,
            last_seq: None,
            started_at,
            finished_at,
            detail: Some(format!("{e:#}")),
        },
    };

    // §9.4: record when, which generation, last sequence, outcome,
    // failure detail — in the live DB.
    {
        let conn = live_db.lock().expect("db mutex poisoned");
        conn.execute(
            "INSERT INTO verify_restore_runs
                 (started_at, finished_at, generation, last_seq, outcome, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                outcome.started_at,
                outcome.finished_at,
                outcome.generation,
                outcome.last_seq,
                if outcome.ok { "ok" } else { "failed" },
                outcome.detail,
            ],
        )
        .context("recording verify-restore run")?;
    }
    Ok(outcome)
}

/// The §9.4 assertions, in order. Returns (generation, last_seq).
async fn check_chain(
    target: &Target,
    key: &[u8; KEY_LEN],
    cfg: &DurabilityConfig,
    work_dir: &Path,
) -> anyhow::Result<(String, u64)> {
    // Download, decrypt, apply ALL changesets in order (temp location,
    // cleaned up when `replayed` drops — §9.6).
    let replayed = restore::fetch_and_replay(target, key, work_dir).await?;

    // Open the result as SQLite, READ-ONLY (§9.6), and integrity-check.
    let conn = Connection::open_with_flags(
        &replayed.db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if integrity != "ok" {
        bail!("integrity_check failed: {integrity}");
    }

    // Schema version known to this binary.
    let schema = crate::db::assert_schema_known(&conn)?;
    if schema != replayed.schema {
        bail!(
            "generation id claims schema {} but restored DB is at {schema}",
            replayed.schema
        );
    }

    // Recency: newest artifact in the chain no older than 2x the
    // snapshot interval. DECISION: the snapshot cadence is the recency
    // clock for both tiers — empty changesets ship nothing (§9.3), so
    // a quiet-but-healthy changeset tier must not read as stale.
    let age = now_secs() - replayed.newest_artifact_at;
    let bound = 2 * cfg.snapshot_interval_secs as i64;
    if age > bound {
        bail!("chain is stale: newest artifact is {age}s old (bound {bound}s)");
    }

    // Restore-fencing epoch marker present and readable at the target
    // (§9.5).
    target
        .read_epoch()
        .await?
        .context("epoch marker absent at target (§9.5 fencing broken)")?;

    Ok((replayed.generation, replayed.last_seq))
}

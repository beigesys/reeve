//! C6 durability integration tests (spec/reeve/07-durability.md):
//! snapshot -> restore round-trip, changeset extract/apply round-trip,
//! generation cutting, atomic-finalize visibility, verify-restore
//! happy and corrupted-object paths, epoch increment-then-serve
//! ordering, and the restore-at-bootstrap DR e2e — all against a local
//! filesystem target (the test + air-gap tier, §9.2).

use std::path::Path;

use reeve_server::config::{Config, DurabilityTier};
use reeve_server::durability::restore::fetch_and_replay;
use reeve_server::durability::target::Target;
use reeve_server::{durability, keyfile};
use rusqlite::OptionalExtension as _;

fn config(data_dir: &Path, target_dir: &Path, tier: DurabilityTier) -> Config {
    Config::from_lookup(|k| match k {
        "REEVE_DATA_DIR" => Some(data_dir.to_string_lossy().into_owned()),
        "REEVE_AUTH" => Some("none".into()),
        "REEVE_DURABILITY" => Some(
            match tier {
                DurabilityTier::None => "none",
                DurabilityTier::Snapshot => "snapshot",
                DurabilityTier::Changeset => "changeset",
            }
            .into(),
        ),
        "REEVE_DURABILITY_TARGET" if tier != DurabilityTier::None => {
            Some(target_dir.to_string_lossy().into_owned())
        }
        // Always due in tests: every ship tick extracts.
        "REEVE_DURABILITY_CHANGESET_INTERVAL_SECS" => Some("0".into()),
        _ => None,
    })
    .expect("config")
}

fn set_setting(state: &reeve_server::state::AppState, key: &str, value: &str) {
    let conn = state.db.lock().unwrap();
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, value],
    )
    .unwrap();
}

fn get_setting(db_path: &Path, key: &str) -> Option<String> {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.query_row(
        "SELECT value FROM settings WHERE key = ?1",
        rusqlite::params![key],
        |r| r.get(0),
    )
    .optional()
    .unwrap()
}

/// §9.2 + §9.5: snapshot a live server, replay from the target, and
/// find the data — the fundamental round-trip.
#[tokio::test]
async fn snapshot_restore_roundtrip() {
    let data = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let cfg = config(data.path(), target_dir.path(), DurabilityTier::Snapshot);
    let state = reeve_server::bootstrap(cfg).unwrap();

    set_setting(&state, "roundtrip", "hello");
    // Tree history restores WITH the snapshot (§9.5: same DB).
    state
        .revisions
        .lock()
        .unwrap()
        .commit(
            [("layers/00-all/app.yml", b"x: 1\n".as_slice())],
            "test",
            "seed",
            revision_store::Stream::Local,
        )
        .unwrap();

    let generation = state.durability.snapshot_now().await.unwrap().unwrap();

    let key = keyfile::load(&data.path().join("secret.key")).unwrap();
    let target = Target::open(target_dir.path().to_str().unwrap(), "default").unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let replayed = fetch_and_replay(&target, &key, scratch.path()).await.unwrap();

    assert_eq!(replayed.generation, generation);
    assert_eq!(replayed.last_seq, 0, "snapshot tier ships no changesets");
    assert_eq!(
        get_setting(&replayed.db_path, "roundtrip").as_deref(),
        Some("hello")
    );
    let conn = rusqlite::Connection::open(&replayed.db_path).unwrap();
    let revs: i64 = conn
        .query_row("SELECT count(*) FROM revisions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(revs, 1, "revision history restored with the snapshot");
}

/// §9.2 atomicity at the bucket: nothing references a generation until
/// `gen/latest` (written last) points at it, and stray staged/partial
/// objects are invisible to restore.
#[tokio::test]
async fn crashed_upload_is_invisible_to_restore() {
    let data = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let cfg = config(data.path(), target_dir.path(), DurabilityTier::Snapshot);
    let state = reeve_server::bootstrap(cfg).unwrap();

    set_setting(&state, "generation", "good");
    let good = state.durability.snapshot_now().await.unwrap().unwrap();

    // Simulate a process killed mid-shipment of a NEWER generation:
    // payload object present, latest pointer never written…
    let gen_dir = target_dir.path().join("reeve/default/gen");
    std::fs::write(gen_dir.join("99991231T235959999Z-5.db"), b"partial garbage").unwrap();
    // …and a staged temp file a crashed LocalFileSystem put leaves.
    std::fs::write(gen_dir.join("99991231T235959999Z-5.db#staged"), b"junk").unwrap();

    let key = keyfile::load(&data.path().join("secret.key")).unwrap();
    let target = Target::open(target_dir.path().to_str().unwrap(), "default").unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let replayed = fetch_and_replay(&target, &key, scratch.path()).await.unwrap();
    assert_eq!(
        replayed.generation, good,
        "restore must follow gen/latest, never a newer unfinalized object"
    );
    assert_eq!(
        get_setting(&replayed.db_path, "generation").as_deref(),
        Some("good")
    );
}

/// D6/D16 startup sequencing: every startup with a tier enabled cuts a
/// fresh generation (migration or not) — the migration-cuts-generation
/// rule rides the same path.
#[tokio::test]
async fn startup_cuts_a_generation_every_boot() {
    let data = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();

    let count_generations = || {
        let dir = target_dir.path().join("reeve/default/gen");
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".db"))
                    .count()
            })
            .unwrap_or(0)
    };

    // Boot 1: migrations apply (fresh DB) — snapshot cut.
    let cfg = config(data.path(), target_dir.path(), DurabilityTier::Snapshot);
    let state = reeve_server::bootstrap(cfg).unwrap();
    assert!(state.migrated_at_boot, "fresh DB applies migrations");
    durability::startup(&state.durability, state.migrated_at_boot).await;
    assert_eq!(count_generations(), 1);
    drop(state);

    std::thread::sleep(std::time::Duration::from_millis(10)); // distinct genid ms
    // Boot 2: no migration — still a fresh anchor (crash-only chain).
    let cfg = config(data.path(), target_dir.path(), DurabilityTier::Snapshot);
    let state = reeve_server::bootstrap(cfg).unwrap();
    assert!(!state.migrated_at_boot);
    durability::startup(&state.durability, state.migrated_at_boot).await;
    assert_eq!(count_generations(), 2);
}

/// §9.3 changeset tier: commits after the snapshot are captured,
/// sealed, shipped in sequence, and replayed onto the generation
/// anchor; an idle tick ships nothing.
#[cfg(feature = "ext-durability-changeset")]
#[tokio::test]
async fn changeset_extract_apply_roundtrip() {
    let data = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let cfg = config(data.path(), target_dir.path(), DurabilityTier::Changeset);
    let state = reeve_server::bootstrap(cfg).unwrap();

    set_setting(&state, "in_snapshot", "yes");
    let generation = state.durability.snapshot_now().await.unwrap().unwrap();

    // Two committed changes -> two shipped sequences.
    set_setting(&state, "cs", "one");
    state.durability.ship_changesets().await.unwrap();
    set_setting(&state, "cs", "two");
    set_setting(&state, "cs_extra", "row");
    state.durability.ship_changesets().await.unwrap();
    // Idle: empty session => NO upload (§9.3).
    state.durability.ship_changesets().await.unwrap();

    let key = keyfile::load(&data.path().join("secret.key")).unwrap();
    let target = Target::open(target_dir.path().to_str().unwrap(), "default").unwrap();
    let listed = target.list_changesets(&generation).await.unwrap();
    assert_eq!(
        listed.iter().map(|(s, _, _)| *s).collect::<Vec<_>>(),
        vec![1, 2],
        "strictly sequenced from 1, no empty uploads"
    );

    let scratch = tempfile::tempdir().unwrap();
    let replayed = fetch_and_replay(&target, &key, scratch.path()).await.unwrap();
    assert_eq!(replayed.generation, generation);
    assert_eq!(replayed.last_seq, 2);
    assert_eq!(get_setting(&replayed.db_path, "in_snapshot").as_deref(), Some("yes"));
    assert_eq!(get_setting(&replayed.db_path, "cs").as_deref(), Some("two"));
    assert_eq!(get_setting(&replayed.db_path, "cs_extra").as_deref(), Some("row"));

    let status = state.durability.status();
    assert_eq!(status.tier, "changeset");
    assert_eq!(status.last_changeset_seq, Some(2));
    assert_eq!(status.pending_changesets, 0);
}

/// §9.3: a new snapshot cuts the generation — the changeset sequence
/// resets and chains to the NEW anchor; pre-snapshot changes live in
/// the snapshot, not in replayed changesets.
#[cfg(feature = "ext-durability-changeset")]
#[tokio::test]
async fn snapshot_cuts_changeset_generation() {
    let data = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let cfg = config(data.path(), target_dir.path(), DurabilityTier::Changeset);
    let state = reeve_server::bootstrap(cfg).unwrap();

    let gen1 = state.durability.snapshot_now().await.unwrap().unwrap();
    set_setting(&state, "k", "v1");
    state.durability.ship_changesets().await.unwrap();

    std::thread::sleep(std::time::Duration::from_millis(10));
    set_setting(&state, "k", "v2");
    let gen2 = state.durability.snapshot_now().await.unwrap().unwrap();
    assert_ne!(gen1, gen2);

    set_setting(&state, "k", "v3");
    state.durability.ship_changesets().await.unwrap();

    let target = Target::open(target_dir.path().to_str().unwrap(), "default").unwrap();
    let cs2 = target.list_changesets(&gen2).await.unwrap();
    assert_eq!(
        cs2.iter().map(|(s, _, _)| *s).collect::<Vec<_>>(),
        vec![1],
        "sequence restarts at 1 on the new generation"
    );

    let key = keyfile::load(&data.path().join("secret.key")).unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let replayed = fetch_and_replay(&target, &key, scratch.path()).await.unwrap();
    assert_eq!(replayed.generation, gen2);
    assert_eq!(get_setting(&replayed.db_path, "k").as_deref(), Some("v3"));
}

/// §9.4 verify-restore: happy path proves the whole chain and records
/// an `ok` row in the live DB.
#[tokio::test]
async fn verify_restore_happy_path_records_ok() {
    let data = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let cfg = config(data.path(), target_dir.path(), DurabilityTier::Snapshot);
    let state = reeve_server::bootstrap(cfg).unwrap();

    set_setting(&state, "verified", "1");
    state.durability.snapshot_now().await.unwrap();

    let outcome = state.durability.verify_restore().await.unwrap();
    assert!(outcome.ok, "verify failed: {:?}", outcome.detail);
    assert!(outcome.generation.is_some());

    let conn = state.db.lock().unwrap();
    let (outcome_row, generation): (String, Option<String>) = conn
        .query_row(
            "SELECT outcome, generation FROM verify_restore_runs ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(outcome_row, "ok");
    assert_eq!(generation, outcome.generation);
    drop(conn);

    let status = state.durability.status();
    assert_eq!(status.effective_tier(), "snapshot");
    assert!(!status.degraded);
}

/// §9.4 + §9.6: a corrupted object at the target FAILS verification
/// (AEAD detects tamper) and the failure is recorded — a backup is
/// trustworthy only if restore-tested.
#[tokio::test]
async fn verify_restore_detects_corrupted_object() {
    let data = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let cfg = config(data.path(), target_dir.path(), DurabilityTier::Snapshot);
    let state = reeve_server::bootstrap(cfg).unwrap();

    let generation = state.durability.snapshot_now().await.unwrap().unwrap();

    // Flip bytes in the shipped snapshot object.
    let obj = target_dir
        .path()
        .join(format!("reeve/default/gen/{generation}.db"));
    let mut bytes = std::fs::read(&obj).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    std::fs::write(&obj, bytes).unwrap();

    let outcome = state.durability.verify_restore().await.unwrap();
    assert!(!outcome.ok);
    assert!(
        outcome.detail.as_deref().unwrap_or("").contains("corrupt"),
        "detail should name corruption: {:?}",
        outcome.detail
    );

    let conn = state.db.lock().unwrap();
    let row: String = conn
        .query_row(
            "SELECT outcome FROM verify_restore_runs ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(row, "failed");
    drop(conn);

    // §9.4: never-successfully-verified reads as NO effective tier.
    assert_eq!(state.durability.status().effective_tier(), "none (unverified)");
}

/// §9.5 restore-at-bootstrap e2e + epoch fencing: seed a server, ship,
/// lose the machine, restore into a fresh data dir (keyfile + target =
/// the two DR artifacts). The epoch marker at the target is incremented
/// BEFORE the DB is placed, the placed DB carries the fenced epoch, and
/// epochs are never reused across successive restores.
#[tokio::test]
async fn restore_at_bootstrap_e2e_with_epoch_fencing() {
    let data_a = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();

    // Server A: seed + ship.
    let cfg_a = config(data_a.path(), target_dir.path(), DurabilityTier::Snapshot);
    let state_a = reeve_server::bootstrap(cfg_a.clone()).unwrap();
    set_setting(&state_a, "seeded", "42");
    state_a
        .revisions
        .lock()
        .unwrap()
        .commit(
            [("layers/00-all/x.yml", b"a: 1\n".as_slice())],
            "test",
            "seed",
            revision_store::Stream::Local,
        )
        .unwrap();
    state_a.durability.snapshot_now().await.unwrap();

    // Restore refuses to clobber a live DB.
    let err = durability::maybe_restore_at_bootstrap(&cfg_a, true)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("already exists"));
    drop(state_a);

    // "Machine lost": fresh data dir B with only the keyfile restored.
    let data_b = tempfile::tempdir().unwrap();
    std::fs::copy(
        data_a.path().join("secret.key"),
        data_b.path().join("secret.key"),
    )
    .unwrap();
    let cfg_b = config(data_b.path(), target_dir.path(), DurabilityTier::Snapshot);

    // Without the keyfile the DR procedure must fail loudly.
    let data_nokey = tempfile::tempdir().unwrap();
    let cfg_nokey = config(data_nokey.path(), target_dir.path(), DurabilityTier::Snapshot);
    let err = durability::maybe_restore_at_bootstrap(&cfg_nokey, true)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("keyfile"), "{err:#}");

    durability::maybe_restore_at_bootstrap(&cfg_b, true).await.unwrap();

    // Epoch marker incremented at the target (§9.5: FIRST, then serve)…
    let epoch_marker =
        std::fs::read_to_string(target_dir.path().join("reeve/default/epoch")).unwrap();
    assert_eq!(epoch_marker.trim(), "1");
    // …and the placed DB already carries the fenced epoch.
    let db_path = data_b.path().join("reeve.db");
    assert_eq!(get_setting(&db_path, "server_epoch").as_deref(), Some("1"));
    assert_eq!(get_setting(&db_path, "seeded").as_deref(), Some("42"));

    // Normal startup continues from the restored DB (migrations
    // idempotent, revision history intact) and allocates manifest
    // versions under the fenced epoch.
    let state_b = reeve_server::bootstrap(cfg_b).unwrap();
    let head = state_b
        .revisions
        .lock()
        .unwrap()
        .head(revision_store::Stream::Local)
        .unwrap();
    assert!(head.is_some(), "tree history restored with the snapshot");
    {
        let conn = state_b.db.lock().unwrap();
        assert_eq!(reeve_server::render::server_epoch(&conn).unwrap(), 1);
    }
    drop(state_b);

    // A second restore (fresh dir C) must NOT reuse the epoch.
    let data_c = tempfile::tempdir().unwrap();
    std::fs::copy(
        data_a.path().join("secret.key"),
        data_c.path().join("secret.key"),
    )
    .unwrap();
    let cfg_c = config(data_c.path(), target_dir.path(), DurabilityTier::Snapshot);
    durability::maybe_restore_at_bootstrap(&cfg_c, true).await.unwrap();
    assert_eq!(
        get_setting(&data_c.path().join("reeve.db"), "server_epoch").as_deref(),
        Some("2"),
        "epoch reuse is forbidden (§9.5)"
    );
}

/// Without the confirmation flag, a fresh server with a configured
/// target starts EMPTY (first-install path) — restore is opt-in DR.
#[tokio::test]
async fn fresh_start_without_flag_does_not_restore() {
    let data = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let cfg = config(data.path(), target_dir.path(), DurabilityTier::Snapshot);
    durability::maybe_restore_at_bootstrap(&cfg, false).await.unwrap();
    assert!(!data.path().join("reeve.db").exists());
}

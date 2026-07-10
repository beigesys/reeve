//! Chaos check (Law 3 extended to the bucket,
//! spec/reeve/07-durability.md §9.2): SIGKILL the server mid-snapshot
//! shipping, then prove the TARGET is still restorable — the latest
//! pointer only ever names a complete, decryptable, integral
//! generation, whatever byte the upload died at.
//!
//! Pattern (same as revision-store/tests/chaos.rs): re-invoke this
//! test binary with env flags; the child seeds data, ships one good
//! generation, signals readiness, then snapshots in a tight loop
//! forever; the parent SIGKILLs it and restores from the target.

use std::process::Command;
use std::time::{Duration, Instant};

use reeve_server::config::{Config, DurabilityTier};
use reeve_server::durability::restore::fetch_and_replay;
use reeve_server::durability::target::Target;
use reeve_server::keyfile;
use rusqlite::OptionalExtension as _;

const CHAOS_DATA_ENV: &str = "REEVE_DURABILITY_CHAOS_DATA";
const CHAOS_TARGET_ENV: &str = "REEVE_DURABILITY_CHAOS_TARGET";
const CHAOS_MARKER_ENV: &str = "REEVE_DURABILITY_CHAOS_MARKER";

fn config(data_dir: &str, target_dir: &str, tier: DurabilityTier) -> Config {
    let data = data_dir.to_string();
    let target = target_dir.to_string();
    Config::from_lookup(move |k| match k {
        "REEVE_DATA_DIR" => Some(data.clone()),
        "REEVE_AUTH" => Some("none".into()),
        "REEVE_DURABILITY" => Some(
            match tier {
                DurabilityTier::None => "none",
                DurabilityTier::Snapshot => "snapshot",
                DurabilityTier::Changeset => "changeset",
            }
            .into(),
        ),
        "REEVE_DURABILITY_TARGET" => Some(target.clone()),
        _ => None,
    })
    .expect("config")
}

/// Child role: seed, ship one generation, signal, then hammer
/// snapshots (with growing payloads so uploads spend real time)
/// until SIGKILLed. Never returns.
fn chaos_child(data_dir: &str, target_dir: &str, marker: &str) -> ! {
    let rt = tokio::runtime::Runtime::new().expect("child: runtime");
    rt.block_on(async {
        let cfg = config(data_dir, target_dir, DurabilityTier::Snapshot);
        let state = reeve_server::bootstrap(cfg).expect("child: bootstrap");
        {
            let conn = state.db.lock().unwrap();
            conn.execute(
                "INSERT INTO settings (key, value) VALUES ('chaos-seed', 'survives')",
                [],
            )
            .expect("child: seed");
        }
        state
            .durability
            .snapshot_now()
            .await
            .expect("child: first snapshot")
            .expect("child: generation id");
        std::fs::write(marker, b"ready").expect("child: marker");

        let mut n: u64 = 0;
        loop {
            n += 1;
            {
                let conn = state.db.lock().unwrap();
                // ~256 KiB of fresh payload per iteration: VACUUM +
                // seal + upload all spend real time, so the SIGKILL
                // lands mid-shipment with high probability.
                conn.execute(
                    "INSERT INTO settings (key, value) VALUES (?1, ?2)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    rusqlite::params![format!("bulk-{}", n % 4), "x".repeat(256 * 1024)],
                )
                .expect("child: bulk write");
            }
            state
                .durability
                .snapshot_now()
                .await
                .expect("child: loop snapshot");
        }
    });
    unreachable!("chaos child loop never exits");
}

#[test]
fn chaos_kill_nine_mid_snapshot_shipping() {
    // Child re-entry point.
    if let (Ok(data), Ok(target), Ok(marker)) = (
        std::env::var(CHAOS_DATA_ENV),
        std::env::var(CHAOS_TARGET_ENV),
        std::env::var(CHAOS_MARKER_ENV),
    ) {
        chaos_child(&data, &target, &marker);
    }

    let data = tempfile::tempdir().expect("data dir");
    let target_dir = tempfile::tempdir().expect("target dir");
    let marker = data.path().join("ready");

    let exe = std::env::current_exe().expect("current_exe");
    let mut child = Command::new(exe)
        .args(["chaos_kill_nine_mid_snapshot_shipping", "--exact", "--nocapture"])
        .env(CHAOS_DATA_ENV, data.path())
        .env(CHAOS_TARGET_ENV, target_dir.path())
        .env(CHAOS_MARKER_ENV, &marker)
        .spawn()
        .expect("spawn child");

    let deadline = Instant::now() + Duration::from_secs(30);
    while !marker.exists() {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("child never signalled readiness");
        }
        if let Some(status) = child.try_wait().expect("try_wait") {
            panic!("child exited early: {status}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // Let the snapshot loop run so the kill lands mid-shipment.
    std::thread::sleep(Duration::from_millis(400));
    child.kill().expect("kill -9 child");
    child.wait().expect("reap child");

    // The bucket must be restorable NOW (Law 3 at the bucket): the
    // latest pointer names only complete generations; a partial upload
    // is invisible.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let key = keyfile::load(&data.path().join("secret.key")).expect("keyfile");
        let target = Target::open(target_dir.path().to_str().unwrap(), "default").unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let replayed = fetch_and_replay(&target, &key, scratch.path())
            .await
            .expect("restore after kill -9 mid-shipping");

        let conn = rusqlite::Connection::open(&replayed.db_path).unwrap();
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(integrity, "ok", "restored DB integral after kill -9");
        let seed: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key = 'chaos-seed'",
                [],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(seed.as_deref(), Some("survives"));
    });

    // And the LIVE db reopens clean too (plain Law 3): startup is
    // recovery — bootstrap must succeed on the killed data dir.
    let cfg = config(
        data.path().to_str().unwrap(),
        target_dir.path().to_str().unwrap(),
        DurabilityTier::Snapshot,
    );
    let state = reeve_server::bootstrap(cfg).expect("bootstrap after kill -9");
    let conn = state.db.lock().unwrap();
    let seed: String = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'chaos-seed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(seed, "survives");
}

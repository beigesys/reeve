//! Chaos check for the converge loop (CLAUDE.md Law 3, crash-only):
//! SIGKILL the process mid-apply, restart, assert convergence
//! resumes to the terminal phase.
//!
//! Pattern (revision-store tests/chaos.rs): the test re-invokes its
//! own binary with env flags. The child starts a converge pass with
//! a provider that signals "apply in flight" via a marker file and
//! then hangs forever; the parent waits for the marker, SIGKILLs the
//! child (the D5 phase row is already committed at `applying`), then
//! re-runs converge in-process with a fast provider and asserts the
//! app reaches `applied` — D5: startup re-runs any non-terminal
//! phase; re-running is a no-op-safe because the provider action
//! (`up -d`) is idempotent.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use reeve_agent::converge::{APPLIED_DIR, content_hash_dir};
use reeve_agent::provider::{AppStatus, Provider, ProviderError};
use reeve_agent::{AgentDb, BundleStore, converge, resolve_desired};
use reeve_types::margo::status::DeploymentState;

const CHAOS_DATA_ENV: &str = "REEVE_AGENT_CHAOS_DATA";
const CHAOS_MARKER_ENV: &str = "REEVE_AGENT_CHAOS_MARKER";

/// Provider whose apply signals the parent then hangs until killed —
/// the SIGKILL therefore always lands strictly inside the `applying`
/// phase.
struct HangingProvider {
    marker: PathBuf,
}

impl Provider for HangingProvider {
    fn apply(&self, _app_dir: &Path) -> Result<AppStatus, ProviderError> {
        fs::write(&self.marker, b"applying").expect("child: marker");
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
    fn remove(&self, _retained_dir: &Path) -> Result<(), ProviderError> {
        unreachable!("chaos child never removes")
    }
    fn status(&self, _app_dir: &Path) -> Result<AppStatus, ProviderError> {
        unreachable!()
    }
}

/// Fast provider for the recovery pass; records invocations.
#[derive(Default)]
struct FastProvider {
    calls: Mutex<Vec<String>>,
}

impl Provider for FastProvider {
    fn apply(&self, app_dir: &Path) -> Result<AppStatus, ProviderError> {
        self.calls
            .lock()
            .unwrap()
            .push(app_dir.file_name().unwrap().to_string_lossy().into_owned());
        Ok(AppStatus {
            state: DeploymentState::Installed,
            detail: None,
        })
    }
    fn remove(&self, _retained_dir: &Path) -> Result<(), ProviderError> {
        Ok(())
    }
    fn status(&self, _app_dir: &Path) -> Result<AppStatus, ProviderError> {
        Ok(AppStatus {
            state: DeploymentState::Installed,
            detail: None,
        })
    }
}

/// Author the on-disk state B2 leaves behind: a complete bundle dir
/// + the `bundle` symlink.
fn author_bundle(data_dir: &Path) {
    let root = data_dir.join("bundles").join("cafe");
    let app = root.join("apps").join("web");
    fs::create_dir_all(&app).expect("bundle dirs");
    fs::write(root.join("manifest.yaml"), "deviceId: dev-1\n").unwrap();
    fs::write(
        app.join("compose.yml"),
        "services:\n  api:\n    image: example/api\n    env_file: [env/api.env]\n",
    )
    .unwrap();
    fs::write(
        app.join("deployment.yaml"),
        "apiVersion: application.margo.org/v1alpha1\n\
         kind: ApplicationDeployment\n\
         id: 99999999-0000-0000-0000-000000000000\n\
         metadata:\n  name: web-deploy\n\
         spec:\n  applicationId: web\n  deploymentProfile:\n    type: docker-compose\n    components:\n      - name: web-stack\n",
    )
    .unwrap();
    let link = data_dir.join("bundle");
    let _ = fs::remove_file(&link);
    std::os::unix::fs::symlink(Path::new("bundles").join("cafe"), &link).unwrap();
}

/// Child role: run one converge pass with the hanging provider.
/// Never returns normally — the parent SIGKILLs it mid-`applying`.
fn chaos_child(data_dir: &str, marker: &str) -> ! {
    let data_dir = PathBuf::from(data_dir);
    let mut db = AgentDb::open(&data_dir.join("agent.db")).expect("child: open db");
    let store = BundleStore::open(&data_dir).expect("child: open store");
    let provider = HangingProvider {
        marker: PathBuf::from(marker),
    };
    let desired = resolve_desired(&db, &store);
    converge(&mut db, &data_dir, &provider, &desired);
    unreachable!("child: converge returned before SIGKILL");
}

#[test]
fn chaos_kill_nine_mid_apply_resumes_to_applied() {
    // Child re-entry point.
    if let (Ok(data), Ok(marker)) = (
        std::env::var(CHAOS_DATA_ENV),
        std::env::var(CHAOS_MARKER_ENV),
    ) {
        chaos_child(&data, &marker);
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().to_path_buf();
    let marker = data_dir.join("apply-in-flight");
    author_bundle(&data_dir);

    let exe = std::env::current_exe().expect("current_exe");
    let mut child = Command::new(exe)
        .args([
            "chaos_kill_nine_mid_apply_resumes_to_applied",
            "--exact",
            "--nocapture",
        ])
        .env(CHAOS_DATA_ENV, &data_dir)
        .env(CHAOS_MARKER_ENV, &marker)
        .spawn()
        .expect("spawn child");

    // Wait until the child is provably inside provider.apply (the
    // `applying` phase row is committed before that call, D5).
    let deadline = Instant::now() + Duration::from_secs(30);
    while !marker.exists() {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("child never reached apply");
        }
        if let Some(status) = child.try_wait().expect("try_wait") {
            panic!("child exited early: {status}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.kill().expect("SIGKILL child"); // kill -9, no ceremony
    child.wait().expect("reap child");

    // Startup IS recovery: reopen everything, assert the crash left
    // resumable non-terminal state.
    let mut db = AgentDb::open(&data_dir.join("agent.db")).expect("reopen db");
    let store = BundleStore::open(&data_dir).expect("reopen store");
    store.recover(&mut db).expect("store recovery");
    let phases: BTreeMap<String, String> = db
        .applied_apps()
        .expect("applied apps")
        .into_iter()
        .map(|a| (a.app_id, a.phase))
        .collect();
    assert_eq!(
        phases.get("web").map(String::as_str),
        Some("applying"),
        "kill -9 must land mid-phase with intent already durable"
    );

    // Re-run converge: the non-terminal phase re-runs to terminal.
    let provider = FastProvider::default();
    let desired = resolve_desired(&db, &store);
    let reports = converge(&mut db, &data_dir, &provider, &desired);
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].app_id, "web");
    assert_eq!(reports[0].state, DeploymentState::Installed);
    assert_eq!(provider.calls.lock().unwrap().as_slice(), ["web"]);

    let apps = db.applied_apps().expect("applied apps");
    assert_eq!(apps[0].phase, "applied");
    assert_eq!(
        apps[0].content_hash,
        content_hash_dir(&data_dir.join("bundle/apps/web")).unwrap(),
        "recorded hash matches the bundle after recovery"
    );
    // Postconditions all hold: retained copy + env file exist.
    assert!(data_dir.join(APPLIED_DIR).join("web/compose.yml").is_file());
    assert!(data_dir.join("apps/web/env/api.env").is_file());

    // And the pass after that is a silent no-op (idempotent
    // recovery — re-running a completed phase changes nothing).
    let desired = resolve_desired(&db, &store);
    assert!(converge(&mut db, &data_dir, &provider, &desired).is_empty());
    assert_eq!(provider.calls.lock().unwrap().len(), 1);
}

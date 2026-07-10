//! `reeve-agent install` / `uninstall` (build item B8; core,
//! unconditional).
//!
//! Normative source: spec/reeve/08-packaging.md §10.3:
//! - install MUST create the system user, write the systemd unit
//!   (via [`crate::systemd`]), write the config SHAPE, and enable +
//!   start the unit; uninstall reverses it;
//! - both MUST be idempotent — Law 3 applies to installers:
//!   re-running on a half-installed box (killed mid-install)
//!   converges to installed, never errors on "already exists",
//!   never duplicates units or users;
//! - installers MUST NOT bake secrets into world-readable files
//!   (the unit references the credential file `agent.toml`, 0600).
//!
//! Every syscall-shaped side effect (user/group management,
//! systemctl) goes through the [`Sys`] trait so the whole install
//! plan is table-testable against a fake with a temp-dir
//! [`InstallLayout`] — tests never need root or a real systemd.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use tracing::info;

use crate::systemd::{AGENT_UNIT, AGENT_USER, ROLLBACK_UNIT, UnitPaths, agent_unit, rollback_unit};
use crate::update::BinDir;

/// Exit status + captured output of one [`Sys::run`] invocation.
#[derive(Debug, Clone)]
pub struct CmdOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOutput {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
}

/// The injected syscall/command layer: external commands and the
/// effective uid. Production is [`RealSys`]; tests fake it.
pub trait Sys {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CmdOutput>;
    fn euid(&self) -> u32;
}

/// Real commands via `std::process::Command`; euid via
/// `libc::geteuid` (always safe to call).
pub struct RealSys;

impl Sys for RealSys {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CmdOutput> {
        let out = std::process::Command::new(program).args(args).output()?;
        Ok(CmdOutput {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn euid(&self) -> u32 {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }
}

/// Installation paths, all under one root (`/` in production; a temp
/// dir in tests and for staged installs). Keeping every path derived
/// from the root means a rooted install is fully self-consistent —
/// the emitted units reference the rooted paths.
#[derive(Debug, Clone)]
pub struct InstallLayout {
    root: PathBuf,
}

impl InstallLayout {
    /// The production layout (root = `/`).
    pub fn system() -> Self {
        Self::under(Path::new("/"))
    }

    pub fn under(root: &Path) -> Self {
        InstallLayout {
            root: root.to_path_buf(),
        }
    }

    fn p(&self, abs: &str) -> PathBuf {
        self.root.join(abs.trim_start_matches('/'))
    }

    /// `/etc/reeve-agent`
    pub fn config_dir(&self) -> PathBuf {
        self.p("etc/reeve-agent")
    }

    /// `/etc/reeve-agent/agent.toml` (docs/decisions/agent.md D4).
    pub fn config_path(&self) -> PathBuf {
        self.config_dir().join("agent.toml")
    }

    /// `/var/lib/reeve-agent`
    pub fn data_dir(&self) -> PathBuf {
        self.p("var/lib/reeve-agent")
    }

    /// `/usr/local/lib/reeve-agent` — the A/B binary dir
    /// ([`crate::update::BinDir`]).
    pub fn lib_dir(&self) -> PathBuf {
        self.p("usr/local/lib/reeve-agent")
    }

    /// `/usr/local/bin/reeve-agent` — operator-convenience symlink
    /// to `<lib>/current`.
    pub fn bin_symlink(&self) -> PathBuf {
        self.p("usr/local/bin/reeve-agent")
    }

    /// `/etc/systemd/system/<unit>`
    pub fn unit_path(&self, unit: &str) -> PathBuf {
        self.p("etc/systemd/system").join(unit)
    }

    /// Paths baked into the emitted units.
    pub fn unit_paths(&self) -> UnitPaths {
        UnitPaths {
            lib_dir: self.lib_dir(),
            config_path: self.config_path(),
            data_dir: self.data_dir(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("reeve-agent {0} must run as root (try sudo)")]
    NotRoot(&'static str),
    #[error("{step}: {source}")]
    Io {
        step: String,
        source: std::io::Error,
    },
    #[error("`{cmd}` failed (exit {code}): {stderr}")]
    Cmd {
        cmd: String,
        code: i32,
        stderr: String,
    },
}

fn io_err(step: impl Into<String>) -> impl FnOnce(std::io::Error) -> InstallError {
    let step = step.into();
    move |source| InstallError::Io { step, source }
}

/// Inputs of `reeve-agent install` beyond the layout.
#[derive(Debug, Clone)]
pub struct InstallOpts {
    /// The binary to install — `std::env::current_exe()` in
    /// production ("a binary copied to a bare box runs", §10.1; the
    /// binary being executed installs ITSELF).
    pub source_binary: PathBuf,
    /// Version staged as `reeve-agent-<version>`
    /// (`CARGO_PKG_VERSION` in production).
    pub version: String,
}

/// `useradd` exit code for "user already exists" — tolerated (Law 3:
/// never error on already-exists; covers a create/create race).
const USERADD_EXISTS: i32 = 9;
/// `userdel` exit code for "no such user" — tolerated on uninstall.
const USERDEL_MISSING: i32 = 6;

/// Run one command, tolerating the listed exit codes in addition to
/// 0. Returns the output for callers that branch on it.
fn run_tolerating(
    sys: &dyn Sys,
    program: &str,
    args: &[&str],
    tolerated: &[i32],
) -> Result<CmdOutput, InstallError> {
    let out = sys
        .run(program, args)
        .map_err(io_err(format!("cannot run {program}")))?;
    if out.ok() || tolerated.contains(&out.code) {
        return Ok(out);
    }
    Err(InstallError::Cmd {
        cmd: format!("{program} {}", args.join(" ")),
        code: out.code,
        stderr: out.stderr.trim().to_string(),
    })
}

/// Atomic file write: temp + fsync + rename in the destination dir
/// (Law 3), with an explicit mode.
fn write_atomic(path: &Path, contents: &str, mode: u32) -> std::io::Result<()> {
    let dir = path.parent().expect("install paths always have a parent");
    fs::create_dir_all(dir)?;
    let name = path.file_name().unwrap().to_string_lossy();
    let tmp = dir.join(format!(".{name}.tmp"));
    let mut open = fs::OpenOptions::new();
    open.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        open.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mut f = open.open(&tmp)?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, path)?;
    File::open(dir)?.sync_all()
}

/// The config SHAPE written when no config exists (§10.3 "write
/// config shape"; CLAUDE.md .env rule: shape, never values — real
/// values are written by `reeve-agent enroll`, which atomically
/// replaces this file).
pub const CONFIG_SHAPE: &str = "\
# reeve-agent configuration (docs/decisions/agent.md D4).
# This file holds the SHAPE only; run
#   reeve-agent enroll --server <URL> --token <JOIN_TOKEN>
# to enroll this device — enrollment rewrites this file (0600,
# atomic) with the issued credentials. Never commit values.
#
# server = \"https://reeve.example\"   # or dir:///path (air-gap media)
# device_token = \"\"                   # issued at enroll — the ONE credential
# device_id = \"\"                      # issued at enroll
# poll_interval_secs = 30
# data_dir = \"/var/lib/reeve-agent\"
# install_dir = \"/usr/local/lib/reeve-agent\"
";

/// Idempotent install (§10.3). Returns the human-readable action
/// log; every step is a converge — re-running on any half-installed
/// state completes it without duplicating anything.
pub fn install(
    sys: &dyn Sys,
    layout: &InstallLayout,
    opts: &InstallOpts,
) -> Result<Vec<String>, InstallError> {
    if sys.euid() != 0 {
        return Err(InstallError::NotRoot("install"));
    }
    let mut actions = Vec::new();
    let mut act = |s: String| {
        info!("{s}");
        actions.push(s);
    };

    // 1. System user (§10.3 "create its system user"). getent exit
    //    2 = no such key; anything else nonzero is a real failure.
    let user = run_tolerating(sys, "getent", &["passwd", AGENT_USER], &[2])?;
    if user.ok() {
        act(format!("user {AGENT_USER}: exists"));
    } else {
        let data_dir = layout.data_dir();
        let data = data_dir.to_string_lossy();
        run_tolerating(
            sys,
            "useradd",
            &[
                "--system",
                "--user-group",
                "--home-dir",
                &data,
                "--no-create-home",
                "--shell",
                "/usr/sbin/nologin",
                AGENT_USER,
            ],
            &[USERADD_EXISTS],
        )?;
        act(format!("user {AGENT_USER}: created"));
    }
    // Compose provider shells out to docker (docs/decisions/agent.md
    // D5): join the docker group when it exists. usermod -aG is
    // idempotent.
    let docker_group = run_tolerating(sys, "getent", &["group", "docker"], &[2])?;
    if docker_group.ok() {
        run_tolerating(sys, "usermod", &["-aG", "docker", AGENT_USER], &[])?;
        act(format!("user {AGENT_USER}: in docker group"));
    } else {
        act("docker group absent; skipped membership".to_string());
    }

    // 2. Directories, owned by the agent user (the daemon runs
    //    unprivileged and self-update writes the lib dir).
    for dir in [layout.config_dir(), layout.data_dir(), layout.lib_dir()] {
        fs::create_dir_all(&dir).map_err(io_err(format!("cannot create {}", dir.display())))?;
    }
    let owner = format!("{AGENT_USER}:{AGENT_USER}");
    let data_dir = layout.data_dir();
    let lib_dir = layout.lib_dir();
    run_tolerating(
        sys,
        "chown",
        &[&owner, &data_dir.to_string_lossy(), &lib_dir.to_string_lossy()],
        &[],
    )?;
    act("directories: present, agent-owned".to_string());

    // 3. A/B binary install: stage THIS binary as
    //    reeve-agent-<version> and point `current` at it — the same
    //    machinery self-update uses (§10.5), so install IS the first
    //    swap. Skips cleanly when already current.
    let bytes = fs::read(&opts.source_binary)
        .map_err(io_err(format!("cannot read {}", opts.source_binary.display())))?;
    let digest = format!("sha256:{:x}", <sha2::Sha256 as sha2::Digest>::digest(&bytes));
    let bin = BinDir::new(&lib_dir);
    bin.stage(&opts.version, &bytes, &digest)
        .map_err(|e| InstallError::Io {
            step: "stage agent binary".into(),
            source: std::io::Error::other(e),
        })?;
    bin.swap_to(&opts.version).map_err(|e| InstallError::Io {
        step: "point current symlink".into(),
        source: std::io::Error::other(e),
    })?;
    // Owned by the agent user so a self-update can replace it.
    run_tolerating(sys, "chown", &["-R", &owner, &lib_dir.to_string_lossy()], &[])?;
    // Operator-convenience PATH symlink -> <lib>/current.
    let bin_link = layout.bin_symlink();
    ensure_symlink(&lib_dir.join(crate::update::CURRENT_LINK), &bin_link)
        .map_err(io_err(format!("cannot link {}", bin_link.display())))?;
    act(format!("binary: reeve-agent-{} current", opts.version));

    // 4. Config shape (§10.3): only when absent — enrollment (or the
    //    operator) owns the real contents; install never clobbers
    //    them. 0600 + agent-owned either way (the token must stay
    //    out of world-readable files, and the daemon must read it).
    let config = layout.config_path();
    if config.exists() {
        act("config: exists, kept".to_string());
    } else {
        write_atomic(&config, CONFIG_SHAPE, 0o600)
            .map_err(io_err(format!("cannot write {}", config.display())))?;
        act("config: shape written".to_string());
    }
    run_tolerating(sys, "chown", &[&owner, &config.to_string_lossy()], &[])?;

    // 5. Units, content-compared so an unchanged re-run skips the
    //    daemon-reload (idempotence you can observe).
    let paths = layout.unit_paths();
    let mut units_changed = false;
    for (name, unit) in [(AGENT_UNIT, agent_unit(&paths)), (ROLLBACK_UNIT, rollback_unit(&paths))] {
        let path = layout.unit_path(name);
        let rendered = unit.render();
        if fs::read_to_string(&path).ok().as_deref() == Some(rendered.as_str()) {
            act(format!("unit {name}: unchanged"));
            continue;
        }
        write_atomic(&path, &rendered, 0o644)
            .map_err(io_err(format!("cannot write {}", path.display())))?;
        act(format!("unit {name}: written"));
        units_changed = true;
    }
    if units_changed {
        run_tolerating(sys, "systemctl", &["daemon-reload"], &[])?;
        act("systemd: daemon-reload".to_string());
    }

    // 6. Enable + start (§10.3). Start only once enrolled — a
    //    shape-only config cannot run and would just flap the unit;
    //    enroll's final instruction is to start the agent.
    run_tolerating(sys, "systemctl", &["enable", AGENT_UNIT], &[])?;
    act(format!("systemd: {AGENT_UNIT} enabled"));
    if crate::config::AgentConfig::from_path(&config).is_ok() {
        run_tolerating(sys, "systemctl", &["start", AGENT_UNIT], &[])?;
        act(format!("systemd: {AGENT_UNIT} started"));
    } else {
        act(format!(
            "systemd: start skipped — not enrolled yet (run `reeve-agent enroll`, then `systemctl start {AGENT_UNIT}`)"
        ));
    }

    Ok(actions)
}

/// Reverse of [`install`] (§10.3). Idempotent: running on a box that
/// was never installed succeeds and reports nothing to do. State
/// (config + data) survives unless `purge` — an uninstall/reinstall
/// must not silently destroy a device identity.
pub fn uninstall(
    sys: &dyn Sys,
    layout: &InstallLayout,
    purge: bool,
) -> Result<Vec<String>, InstallError> {
    if sys.euid() != 0 {
        return Err(InstallError::NotRoot("uninstall"));
    }
    let mut actions = Vec::new();
    let mut act = |s: String| {
        info!("{s}");
        actions.push(s);
    };

    // Stop/disable tolerate every failure shape systemd produces for
    // "not loaded"/"not enabled" (codes vary by version) — uninstall
    // of a half-installed box must proceed (Law 3).
    for verb in ["stop", "disable"] {
        if let Ok(out) = sys.run("systemctl", &[verb, AGENT_UNIT])
            && out.ok()
        {
            act(format!("systemd: {AGENT_UNIT} {verb}ped").replace("disableped", "disabled"));
        }
    }

    let mut units_removed = false;
    for name in [AGENT_UNIT, ROLLBACK_UNIT] {
        let path = layout.unit_path(name);
        match fs::remove_file(&path) {
            Ok(()) => {
                act(format!("unit {name}: removed"));
                units_removed = true;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(format!("cannot remove {}", path.display()))(e)),
        }
    }
    if units_removed {
        run_tolerating(sys, "systemctl", &["daemon-reload"], &[])?;
        act("systemd: daemon-reload".to_string());
    }

    for path in [layout.bin_symlink()] {
        match fs::remove_file(&path) {
            Ok(()) => act(format!("removed {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(format!("cannot remove {}", path.display()))(e)),
        }
    }
    match fs::remove_dir_all(layout.lib_dir()) {
        Ok(()) => act(format!("removed {}", layout.lib_dir().display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err(format!("cannot remove {}", layout.lib_dir().display()))(e)),
    }

    run_tolerating(sys, "userdel", &[AGENT_USER], &[USERDEL_MISSING])?;
    act(format!("user {AGENT_USER}: removed (if present)"));

    if purge {
        for dir in [layout.config_dir(), layout.data_dir()] {
            match fs::remove_dir_all(&dir) {
                Ok(()) => act(format!("purged {}", dir.display())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_err(format!("cannot purge {}", dir.display()))(e)),
            }
        }
    } else {
        act("kept config + data (use --purge to remove device identity and state)".to_string());
    }

    Ok(actions)
}

/// Idempotent absolute symlink: replace atomically only when the
/// target differs.
fn ensure_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    if fs::read_link(link).ok().as_deref() == Some(target) {
        return Ok(());
    }
    if let Some(dir) = link.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = link.with_file_name(format!(
        ".{}.tmp",
        link.file_name().unwrap().to_string_lossy()
    ));
    let _ = fs::remove_file(&tmp);
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, &tmp)?;
    #[cfg(not(unix))]
    return Err(std::io::Error::other("symlinks require unix"));
    #[cfg(unix)]
    {
        fs::rename(&tmp, link)?;
        if let Some(dir) = link.parent() {
            File::open(dir)?.sync_all()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// Recording fake `Sys`: getent answers from flags, everything
    /// else succeeds (tests MUST NOT require root or systemd).
    struct FakeSys {
        cmds: RefCell<Vec<String>>,
        euid: u32,
        user_exists: Cell<bool>,
        docker_group: bool,
    }

    impl FakeSys {
        fn root() -> Self {
            FakeSys {
                cmds: RefCell::new(Vec::new()),
                euid: 0,
                user_exists: Cell::new(false),
                docker_group: true,
            }
        }
        fn cmds(&self) -> Vec<String> {
            self.cmds.borrow().clone()
        }
        fn ran(&self, needle: &str) -> usize {
            self.cmds().iter().filter(|c| c.contains(needle)).count()
        }
    }

    impl Sys for FakeSys {
        fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CmdOutput> {
            let cmd = format!("{program} {}", args.join(" "));
            self.cmds.borrow_mut().push(cmd.clone());
            let code = match (program, args.first().copied()) {
                ("getent", Some("passwd")) => {
                    if self.user_exists.get() {
                        0
                    } else {
                        2
                    }
                }
                ("getent", Some("group")) => {
                    if self.docker_group {
                        0
                    } else {
                        2
                    }
                }
                ("useradd", _) => {
                    self.user_exists.set(true);
                    0
                }
                ("userdel", _) => {
                    if self.user_exists.get() {
                        self.user_exists.set(false);
                        0
                    } else {
                        USERDEL_MISSING
                    }
                }
                _ => 0,
            };
            Ok(CmdOutput {
                code,
                stdout: String::new(),
                stderr: String::new(),
            })
        }

        fn euid(&self) -> u32 {
            self.euid
        }
    }

    fn opts(root: &Path) -> InstallOpts {
        let src = root.join("reeve-agent-src");
        fs::write(&src, b"the agent binary").unwrap();
        InstallOpts {
            source_binary: src,
            version: "0.1.0".into(),
        }
    }

    #[test]
    fn non_root_is_a_clear_error() {
        let t = tempfile::tempdir().unwrap();
        let layout = InstallLayout::under(t.path());
        let sys = FakeSys {
            euid: 1000,
            ..FakeSys::root()
        };
        let err = install(&sys, &layout, &opts(t.path())).unwrap_err();
        assert!(err.to_string().contains("must run as root"), "{err}");
        assert!(matches!(err, InstallError::NotRoot("install")));
        let err = uninstall(&sys, &layout, false).unwrap_err();
        assert!(matches!(err, InstallError::NotRoot("uninstall")));
    }

    #[test]
    fn fresh_install_converges_everything() {
        let t = tempfile::tempdir().unwrap();
        let layout = InstallLayout::under(t.path());
        let sys = FakeSys::root();

        install(&sys, &layout, &opts(t.path())).unwrap();

        // user created once, docker membership granted
        assert_eq!(sys.ran("useradd"), 1);
        assert_eq!(sys.ran("usermod -aG docker reeve-agent"), 1);
        // A/B layout in place: versioned binary + current symlink
        let bin = BinDir::new(&layout.lib_dir());
        assert_eq!(bin.current_target().as_deref(), Some("reeve-agent-0.1.0"));
        assert_eq!(
            fs::read(layout.lib_dir().join("reeve-agent-0.1.0")).unwrap(),
            b"the agent binary"
        );
        assert_eq!(
            fs::read_link(layout.bin_symlink()).unwrap(),
            layout.lib_dir().join("current")
        );
        // config shape, 0600, never a value in it
        let config = fs::read_to_string(layout.config_path()).unwrap();
        assert_eq!(config, CONFIG_SHAPE);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(layout.config_path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // both units written, reload + enable ran; start skipped
        // (shape-only config = not enrolled)
        assert!(layout.unit_path(AGENT_UNIT).is_file());
        assert!(layout.unit_path(ROLLBACK_UNIT).is_file());
        assert_eq!(sys.ran("daemon-reload"), 1);
        assert_eq!(sys.ran("systemctl enable reeve-agent.service"), 1);
        assert_eq!(sys.ran("systemctl start"), 0, "no start before enrollment");
    }

    #[test]
    fn reinstall_is_idempotent_no_duplicates_no_reload() {
        let t = tempfile::tempdir().unwrap();
        let layout = InstallLayout::under(t.path());
        let sys = FakeSys::root();
        install(&sys, &layout, &opts(t.path())).unwrap();
        let first_unit = fs::read_to_string(layout.unit_path(AGENT_UNIT)).unwrap();

        install(&sys, &layout, &opts(t.path())).unwrap();

        assert_eq!(sys.ran("useradd"), 1, "user never created twice");
        assert_eq!(sys.ran("daemon-reload"), 1, "unchanged units skip reload");
        assert_eq!(
            fs::read_to_string(layout.unit_path(AGENT_UNIT)).unwrap(),
            first_unit
        );
        // exactly one versioned binary — no duplicates
        let binaries: Vec<_> = fs::read_dir(layout.lib_dir())
            .unwrap()
            .filter_map(|e| {
                let n = e.unwrap().file_name().to_string_lossy().into_owned();
                n.starts_with("reeve-agent-").then_some(n)
            })
            .collect();
        assert_eq!(binaries, vec!["reeve-agent-0.1.0"]);
    }

    /// The §10.3 half-installed table: kill -9 landed after step N;
    /// a re-run converges from every cut without erroring.
    #[test]
    fn half_installed_states_converge() {
        let t = tempfile::tempdir().unwrap();
        let layout = InstallLayout::under(t.path());
        let sys = FakeSys::root();
        install(&sys, &layout, &opts(t.path())).unwrap();

        // cut A: unit file lost (crash before unit write)
        fs::remove_file(layout.unit_path(AGENT_UNIT)).unwrap();
        install(&sys, &layout, &opts(t.path())).unwrap();
        assert!(layout.unit_path(AGENT_UNIT).is_file());
        assert_eq!(sys.ran("daemon-reload"), 2, "changed unit reloads");

        // cut B: current symlink lost (crash mid A/B step)
        fs::remove_file(layout.lib_dir().join("current")).unwrap();
        install(&sys, &layout, &opts(t.path())).unwrap();
        let bin = BinDir::new(&layout.lib_dir());
        assert_eq!(bin.current_target().as_deref(), Some("reeve-agent-0.1.0"));

        // cut C: user exists but NOTHING else (enrollment box reuse)
        let t2 = tempfile::tempdir().unwrap();
        let layout2 = InstallLayout::under(t2.path());
        let sys2 = FakeSys::root();
        sys2.user_exists.set(true);
        install(&sys2, &layout2, &opts(t2.path())).unwrap();
        assert_eq!(sys2.ran("useradd"), 0, "existing user untouched");
        assert!(layout2.unit_path(AGENT_UNIT).is_file());
    }

    #[test]
    fn existing_config_is_never_clobbered_and_start_runs_when_enrolled() {
        let t = tempfile::tempdir().unwrap();
        let layout = InstallLayout::under(t.path());
        let sys = FakeSys::root();
        // enrolled config already present (enroll ran first, D4)
        fs::create_dir_all(layout.config_dir()).unwrap();
        fs::write(
            layout.config_path(),
            "server = \"https://reeve.example\"\ndevice_token = \"rvd_x\"\n",
        )
        .unwrap();

        install(&sys, &layout, &opts(t.path())).unwrap();

        let kept = fs::read_to_string(layout.config_path()).unwrap();
        assert!(kept.contains("rvd_x"), "enrolled credentials survive install");
        assert_eq!(sys.ran("systemctl start reeve-agent.service"), 1);
    }

    #[test]
    fn uninstall_reverses_and_tolerates_absence() {
        let t = tempfile::tempdir().unwrap();
        let layout = InstallLayout::under(t.path());
        let sys = FakeSys::root();
        install(&sys, &layout, &opts(t.path())).unwrap();

        uninstall(&sys, &layout, false).unwrap();
        assert!(!layout.unit_path(AGENT_UNIT).exists());
        assert!(!layout.unit_path(ROLLBACK_UNIT).exists());
        assert!(!layout.lib_dir().exists());
        assert!(!layout.bin_symlink().exists());
        assert_eq!(sys.ran("userdel reeve-agent"), 1);
        // identity + state kept without --purge
        assert!(layout.config_path().is_file());
        assert!(layout.data_dir().is_dir());

        // re-run on the now-clean box: no errors (Law 3)
        uninstall(&sys, &layout, false).unwrap();

        // purge removes identity + state
        uninstall(&sys, &layout, true).unwrap();
        assert!(!layout.config_dir().exists());
        assert!(!layout.data_dir().exists());

        // uninstall on a NEVER-installed root
        let t2 = tempfile::tempdir().unwrap();
        uninstall(&FakeSys::root(), &InstallLayout::under(t2.path()), true).unwrap();
    }
}

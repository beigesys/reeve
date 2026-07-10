//! Systemd unit emission (build item B8; core, unconditional).
//!
//! The unit-file builder here is the SAME machinery a future
//! systemd-unit `Provider` would use (spec/reeve/08-packaging.md
//! §10.3: "write its systemd unit (via the same unit-emitting
//! machinery as the systemd-unit Provider)") — a plain, deterministic
//! key/value renderer with no installer knowledge; the two reeve
//! units are defined on top of it.
//!
//! Rollback posture (spec/reeve/08-packaging.md §10.5, the boring
//! option — recorded in DECISIONS-MADE.md): the agent unit declares
//! `Restart=always` + `StartLimitIntervalSec/StartLimitBurst` as the
//! "first health window" (a new binary that cannot stay up exhausts
//! the burst and the unit enters failed state) and
//! `OnFailure=reeve-agent-rollback.service`. The rollback unit
//! executes `<lib>/previous rollback` — THROUGH the retained
//! previous binary, so a broken new binary can never prevent its own
//! rollback — which flips the `current` symlink back, writes the
//! hold marker, and restarts the agent unit.

use std::fmt::Write as _;
use std::path::Path;

/// The agent's service unit file name.
pub const AGENT_UNIT: &str = "reeve-agent.service";
/// The §10.5 rollback companion unit file name.
pub const ROLLBACK_UNIT: &str = "reeve-agent-rollback.service";
/// System user the agent runs as (spec/reeve/08-packaging.md §10.3:
/// "create its system user").
pub const AGENT_USER: &str = "reeve-agent";

/// A systemd unit file: named sections of key=value entries,
/// rendered in insertion order (deterministic output — the installer
/// compares rendered text to decide whether a daemon-reload is
/// needed).
#[derive(Debug, Clone, Default)]
pub struct UnitFile {
    sections: Vec<(String, Vec<(String, String)>)>,
}

impl UnitFile {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a new section (e.g. `Unit`, `Service`, `Install`).
    pub fn section(mut self, name: &str) -> Self {
        self.sections.push((name.to_string(), Vec::new()));
        self
    }

    /// Append `key=value` to the most recently started section.
    /// Panics if no section was started — a construction bug, not a
    /// runtime condition.
    pub fn entry(mut self, key: &str, value: impl Into<String>) -> Self {
        self.sections
            .last_mut()
            .expect("UnitFile::entry before UnitFile::section")
            .1
            .push((key.to_string(), value.into()));
        self
    }

    /// Render to unit-file text: `[Section]` headers, `Key=Value`
    /// lines, one blank line between sections, trailing newline.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (i, (name, entries)) in self.sections.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            let _ = writeln!(out, "[{name}]");
            for (k, v) in entries {
                let _ = writeln!(out, "{k}={v}");
            }
        }
        out
    }
}

/// Absolute paths baked into the reeve units. All derived from one
/// [`crate::install::InstallLayout`] so a rooted (test) install is
/// self-consistent.
#[derive(Debug, Clone)]
pub struct UnitPaths {
    /// A/B binary dir holding `current`/`previous` symlinks
    /// ([`crate::update::BinDir`]).
    pub lib_dir: std::path::PathBuf,
    /// `agent.toml` path (referenced via `REEVE_AGENT_CONFIG`; the
    /// unit references the credential FILE and never inlines its
    /// contents — §10.3 "MUST NOT bake secrets into world-readable
    /// files").
    pub config_path: std::path::PathBuf,
    /// Agent state dir (`agent.db`, bundles, applied copies).
    pub data_dir: std::path::PathBuf,
}

fn display(p: &Path) -> String {
    p.display().to_string()
}

/// The reeve-agent service unit (spec/reeve/08-packaging.md §10.3,
/// §10.5). See module docs for the Restart/StartLimit/OnFailure
/// rollback posture.
pub fn agent_unit(paths: &UnitPaths) -> UnitFile {
    let exec = paths.lib_dir.join(crate::update::CURRENT_LINK);
    UnitFile::new()
        .section("Unit")
        .entry("Description", "reeve-agent — fleet desired-state agent (Margo-inspired)")
        .entry("Documentation", "https://github.com/bherbruck/reeve")
        .entry("Wants", "network-online.target")
        .entry("After", "network-online.target docker.service")
        // §10.5 first health window: a new binary that cannot stay
        // up exhausts the burst; the failed unit triggers rollback.
        .entry("StartLimitIntervalSec", "300")
        .entry("StartLimitBurst", "3")
        .entry("OnFailure", ROLLBACK_UNIT)
        .section("Service")
        // ExecStart resolves the A/B `current` symlink at exec time
        // (§10.5: the atomic symlink swap IS the update).
        .entry("ExecStart", display(&exec))
        .entry("User", AGENT_USER)
        .entry("Group", AGENT_USER)
        // The unit references the credential file; the device token
        // lives ONLY in agent.toml (0600, agent-owned) — §10.3.
        .entry("Environment", format!("REEVE_AGENT_CONFIG={}", display(&paths.config_path)))
        // Crash-only (Law 3): always restart — a clean exit is the
        // self-update's re-exec request ([`crate::update::ExitRestarter`]).
        .entry("Restart", "always")
        .entry("RestartSec", "2")
        .entry("NoNewPrivileges", "true")
        .entry("ProtectSystem", "full")
        .entry("ProtectHome", "true")
        // Writable state: the data dir, and the A/B lib dir the
        // self-update stages binaries into.
        .entry(
            "ReadWritePaths",
            format!("{} {}", display(&paths.data_dir), display(&paths.lib_dir)),
        )
        // Operational contract (CLAUDE.md Substrate rules):
        // structured logs to stdout -> journald.
        .entry("StandardOutput", "journal")
        .entry("StandardError", "journal")
        .section("Install")
        .entry("WantedBy", "multi-user.target")
}

/// The §10.5 rollback companion: executed via the RETAINED previous
/// binary (a broken new binary cannot prevent its own rollback);
/// `rollback` flips `current` back, writes the hold marker, and
/// restarts the agent unit. `ConditionPathExists` keeps the unit a
/// no-op on a box that has never self-updated.
pub fn rollback_unit(paths: &UnitPaths) -> UnitFile {
    let previous = paths.lib_dir.join(crate::update::PREVIOUS_LINK);
    UnitFile::new()
        .section("Unit")
        .entry("Description", "reeve-agent A/B rollback (spec/reeve/08-packaging.md §10.5)")
        .entry("ConditionPathExists", display(&previous))
        .section("Service")
        .entry("Type", "oneshot")
        .entry(
            "ExecStart",
            format!("{} rollback --install-dir {}", display(&previous), display(&paths.lib_dir)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn system_paths() -> UnitPaths {
        UnitPaths {
            lib_dir: PathBuf::from("/usr/local/lib/reeve-agent"),
            config_path: PathBuf::from("/etc/reeve-agent/agent.toml"),
            data_dir: PathBuf::from("/var/lib/reeve-agent"),
        }
    }

    #[test]
    fn builder_renders_sections_in_order() {
        let text = UnitFile::new()
            .section("Unit")
            .entry("Description", "x")
            .section("Service")
            .entry("ExecStart", "/bin/true")
            .render();
        assert_eq!(text, "[Unit]\nDescription=x\n\n[Service]\nExecStart=/bin/true\n");
    }

    /// Golden test: the agent unit, byte-exact. This text IS the
    /// spec for what `reeve-agent install` writes.
    #[test]
    fn agent_unit_golden() {
        let expected = "\
[Unit]
Description=reeve-agent — fleet desired-state agent (Margo-inspired)
Documentation=https://github.com/bherbruck/reeve
Wants=network-online.target
After=network-online.target docker.service
StartLimitIntervalSec=300
StartLimitBurst=3
OnFailure=reeve-agent-rollback.service

[Service]
ExecStart=/usr/local/lib/reeve-agent/current
User=reeve-agent
Group=reeve-agent
Environment=REEVE_AGENT_CONFIG=/etc/reeve-agent/agent.toml
Restart=always
RestartSec=2
NoNewPrivileges=true
ProtectSystem=full
ProtectHome=true
ReadWritePaths=/var/lib/reeve-agent /usr/local/lib/reeve-agent
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
";
        assert_eq!(agent_unit(&system_paths()).render(), expected);
    }

    /// Golden test: the rollback companion unit, byte-exact.
    #[test]
    fn rollback_unit_golden() {
        let expected = "\
[Unit]
Description=reeve-agent A/B rollback (spec/reeve/08-packaging.md §10.5)
ConditionPathExists=/usr/local/lib/reeve-agent/previous

[Service]
Type=oneshot
ExecStart=/usr/local/lib/reeve-agent/previous rollback --install-dir /usr/local/lib/reeve-agent
";
        assert_eq!(rollback_unit(&system_paths()).render(), expected);
    }

    /// No secret material can appear in unit text: the only
    /// credential reference is the config FILE path (§10.3).
    #[test]
    fn unit_references_credential_file_never_inlines() {
        let text = agent_unit(&system_paths()).render();
        assert!(text.contains("REEVE_AGENT_CONFIG=/etc/reeve-agent/agent.toml"));
        assert!(!text.to_lowercase().contains("token"));
    }
}

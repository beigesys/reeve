//! Agent configuration — the `/etc/reeve-agent/agent.toml` shape.
//!
//! Written at enroll time (docs/decisions/agent.md D4 step 3: 0600,
//! temp+rename); read at every startup. Config lives in files,
//! settings in the DB (CLAUDE.md Law 4). Path overridable via the
//! `REEVE_AGENT_CONFIG` environment variable (tests, non-root runs).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Default config path (docs/decisions/agent.md D4).
pub const DEFAULT_CONFIG_PATH: &str = "/etc/reeve-agent/agent.toml";
/// Environment variable overriding the config path.
pub const CONFIG_PATH_ENV: &str = "REEVE_AGENT_CONFIG";
/// Default agent data directory (agent.db, applied/ copies).
pub const DEFAULT_DATA_DIR: &str = "/var/lib/reeve-agent";
/// Default manifest poll interval in seconds. The spec pins no
/// value; 30s keeps nudge-free latency tolerable
/// (spec/reeve/02-channel.md: without the channel, latency = poll
/// interval) without hammering constrained WANs (Law 5).
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 30;
/// Default health sample interval in seconds
/// (spec/reeve/05-health-journal.md §7.2 RECOMMENDED default 60 s).
pub const DEFAULT_HEALTH_INTERVAL_SECS: u64 = 60;
/// Default journal retention window in days
/// (spec/reeve/05-health-journal.md §7.1 RECOMMENDED default:
/// 30 days or 512 MiB, whichever first).
pub const DEFAULT_JOURNAL_RETENTION_DAYS: u32 = 30;
/// Default journal size bound in bytes (§7.1: 512 MiB).
pub const DEFAULT_JOURNAL_MAX_BYTES: u64 = 512 * 1024 * 1024;
/// Default A/B binary install dir (B8, spec/reeve/08-packaging.md
/// §10.5): versioned binaries + `current`/`previous` symlinks,
/// written by `reeve-agent install` and the self-updater.
pub const DEFAULT_INSTALL_DIR: &str = "/usr/local/lib/reeve-agent";

/// Errors loading agent configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot parse config {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
}

/// `/etc/reeve-agent/agent.toml`.
///
/// ```toml
/// server = "https://reeve.example"   # or "dir:///opt/reeve-source"
/// device_token = "..."               # issued at enroll (D4); the ONE credential
/// device_id = "dev-abc123"           # issued at enroll (D4)
/// poll_interval_secs = 30
/// data_dir = "/var/lib/reeve-agent"
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Manifest source: `https://…` (a reeve or vanilla Margo
    /// server) or `dir://<path>` (Milestone 1 harness / air-gap
    /// media — a directory with `manifest.json` + OCI layout;
    /// CLAUDE.md Build order).
    pub server: String,
    /// Device bearer token — the ONE credential for API, manifest
    /// poll, /v2 pulls, websocket, secrets resolve
    /// (docs/decisions/agent.md D4). Absent for `dir://` sources.
    #[serde(default)]
    pub device_token: Option<String>,
    /// Device id issued at enroll (docs/decisions/agent.md D4).
    #[serde(default)]
    pub device_id: Option<String>,
    /// Manifest poll interval, seconds.
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    /// Agent state directory: agent.db, retained applied/ copies
    /// (docs/decisions/agent.md D5).
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// Health sample interval, seconds (REV-004,
    /// spec/reeve/05-health-journal.md §7.2). Consumed by the
    /// ext-health sampler; inert without that feature.
    #[serde(default = "default_health_interval")]
    pub health_interval_secs: u64,
    /// Journal retention window, days (§7.1). Age-based eviction
    /// applies to ACKNOWLEDGED records only.
    #[serde(default = "default_journal_retention_days")]
    pub journal_retention_days: u32,
    /// Journal size bound, bytes (§7.1). Crossing it forces
    /// oldest-first eviction — of unacknowledged records too, in
    /// which case a gap mark is journaled.
    #[serde(default = "default_journal_max_bytes")]
    pub journal_max_bytes: u64,
    /// A/B binary dir for self-update (B8,
    /// spec/reeve/08-packaging.md §10.5). Matches where
    /// `reeve-agent install` staged the binary.
    #[serde(default = "default_install_dir")]
    pub install_dir: PathBuf,
}

fn default_poll_interval() -> u64 {
    DEFAULT_POLL_INTERVAL_SECS
}

fn default_data_dir() -> PathBuf {
    PathBuf::from(DEFAULT_DATA_DIR)
}

fn default_health_interval() -> u64 {
    DEFAULT_HEALTH_INTERVAL_SECS
}

fn default_journal_retention_days() -> u32 {
    DEFAULT_JOURNAL_RETENTION_DAYS
}

fn default_journal_max_bytes() -> u64 {
    DEFAULT_JOURNAL_MAX_BYTES
}

fn default_install_dir() -> PathBuf {
    PathBuf::from(DEFAULT_INSTALL_DIR)
}

impl AgentConfig {
    /// Load from `REEVE_AGENT_CONFIG` if set, else
    /// [`DEFAULT_CONFIG_PATH`].
    pub fn load() -> Result<Self, ConfigError> {
        let path = std::env::var_os(CONFIG_PATH_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
        Self::from_path(&path)
    }

    /// Load from an explicit path.
    pub fn from_path(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Path of the agent's SQLite state database inside `data_dir`.
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("agent.db")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.toml");
        std::fs::write(
            &path,
            r#"
server = "https://reeve.example"
device_token = "tok-1"
device_id = "dev-1"
poll_interval_secs = 5
data_dir = "/tmp/reeve-test"
"#,
        )
        .unwrap();
        let cfg = AgentConfig::from_path(&path).unwrap();
        assert_eq!(cfg.server, "https://reeve.example");
        assert_eq!(cfg.device_token.as_deref(), Some("tok-1"));
        assert_eq!(cfg.device_id.as_deref(), Some("dev-1"));
        assert_eq!(cfg.poll_interval_secs, 5);
        assert_eq!(cfg.data_dir, PathBuf::from("/tmp/reeve-test"));
        assert_eq!(cfg.db_path(), PathBuf::from("/tmp/reeve-test/agent.db"));
        // Health knobs default when absent (REV-004 spec defaults).
        assert_eq!(cfg.health_interval_secs, DEFAULT_HEALTH_INTERVAL_SECS);
        assert_eq!(cfg.journal_retention_days, DEFAULT_JOURNAL_RETENTION_DAYS);
        assert_eq!(cfg.journal_max_bytes, DEFAULT_JOURNAL_MAX_BYTES);
        assert_eq!(cfg.install_dir, PathBuf::from(DEFAULT_INSTALL_DIR));
    }

    #[test]
    fn minimal_config_gets_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.toml");
        std::fs::write(&path, "server = \"dir:///opt/src\"\n").unwrap();
        let cfg = AgentConfig::from_path(&path).unwrap();
        assert_eq!(cfg.server, "dir:///opt/src");
        assert_eq!(cfg.device_token, None);
        assert_eq!(cfg.poll_interval_secs, DEFAULT_POLL_INTERVAL_SECS);
        assert_eq!(cfg.data_dir, PathBuf::from(DEFAULT_DATA_DIR));
    }

    #[test]
    fn missing_file_is_read_error() {
        let err = AgentConfig::from_path(Path::new("/nonexistent/agent.toml")).unwrap_err();
        assert!(matches!(err, ConfigError::Read { .. }));
    }

    #[test]
    fn unknown_fields_tolerated() {
        // Forward compatibility: an older agent reading a newer
        // config shape must not fail (mirrors §3.2 degradation).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.toml");
        std::fs::write(&path, "server = \"https://x\"\nfuture_knob = true\n").unwrap();
        assert!(AgentConfig::from_path(&path).is_ok());
    }
}

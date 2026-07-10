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
}

fn default_poll_interval() -> u64 {
    DEFAULT_POLL_INTERVAL_SECS
}

fn default_data_dir() -> PathBuf {
    PathBuf::from(DEFAULT_DATA_DIR)
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

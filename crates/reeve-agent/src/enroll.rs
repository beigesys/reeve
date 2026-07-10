//! `reeve-agent enroll` — the D4 enrollment ceremony, agent side
//! (docs/decisions/agent.md D4 steps 1 and 3):
//!
//! 1. `POST <server>/api/reeve/v1/enroll { join_token, hostname, arch,
//!    agent_version }` (spec/reeve/01-framework.md §3.8 item 1 — the
//!    reeve replacement for Margo onboarding).
//! 2. Write agent.toml with the issued device_id + device token:
//!    0600, temp + fsync + rename (Law 3: atomic writes; a crash leaves
//!    either the old config or the new, never a torn one).
//!
//! Enrollment is the one agent operation that REQUIRES the network —
//! Law 5's continue-from-last-known-state does not apply because there
//! is no prior state; failure here is a clean, retryable error (the
//! server's enroll is idempotent for the same join token + hostname).

use std::path::{Path, PathBuf};

use reeve_types::reeve::enroll::{EnrollRequest, EnrollResponse};

use crate::config::{AgentConfig, DEFAULT_DATA_DIR, DEFAULT_POLL_INTERVAL_SECS};

/// Enrollment endpoint path (docs/decisions/agent.md D4;
/// spec/reeve/01-framework.md §3.8 item 1).
pub const ENROLL_PATH: &str = "/api/reeve/v1/enroll";

/// Inputs of `reeve-agent enroll`.
#[derive(Debug, Clone)]
pub struct EnrollOpts {
    /// Server base URL (`https://reeve.example`).
    pub server: String,
    /// Operator-issued join token (plain or re-enroll, D4).
    pub join_token: String,
    /// Where to write agent.toml.
    pub config_path: PathBuf,
    /// Agent data dir recorded in the config (None => default).
    pub data_dir: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum EnrollCmdError {
    #[error("cannot reach {server}: {detail}")]
    Unreachable { server: String, detail: String },
    #[error("server rejected the join token (invalid, expired, or exhausted)")]
    TokenRejected,
    #[error("enrollment failed: server answered {status}: {detail}")]
    Server { status: u16, detail: String },
    #[error("unparseable enrollment response: {0}")]
    BadResponse(String),
    #[error("cannot write config {path}: {source}")]
    WriteConfig {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Run the enrollment call and write agent.toml. Returns the written
/// config on success.
pub async fn enroll(opts: &EnrollOpts) -> Result<AgentConfig, EnrollCmdError> {
    let server = opts.server.trim_end_matches('/').to_string();
    let request = EnrollRequest {
        join_token: opts.join_token.clone(),
        hostname: detect_hostname(),
        arch: std::env::consts::ARCH.to_string(),
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| EnrollCmdError::Unreachable {
            server: server.clone(),
            detail: e.to_string(),
        })?;
    let resp = client
        .post(format!("{server}{ENROLL_PATH}"))
        .json(&request)
        .send()
        .await
        .map_err(|e| EnrollCmdError::Unreachable {
            server: server.clone(),
            detail: e.to_string(),
        })?;

    let status = resp.status();
    if status.as_u16() == 401 {
        return Err(EnrollCmdError::TokenRejected);
    }
    if !status.is_success() {
        let detail = resp.text().await.unwrap_or_default();
        return Err(EnrollCmdError::Server {
            status: status.as_u16(),
            detail,
        });
    }
    let body: EnrollResponse = resp
        .json()
        .await
        .map_err(|e| EnrollCmdError::BadResponse(e.to_string()))?;

    let cfg = AgentConfig {
        server,
        device_token: Some(body.device_token),
        device_id: Some(body.device_id),
        poll_interval_secs: DEFAULT_POLL_INTERVAL_SECS,
        data_dir: opts
            .data_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_DATA_DIR)),
        health_interval_secs: crate::config::DEFAULT_HEALTH_INTERVAL_SECS,
        journal_retention_days: crate::config::DEFAULT_JOURNAL_RETENTION_DAYS,
        journal_max_bytes: crate::config::DEFAULT_JOURNAL_MAX_BYTES,
        install_dir: PathBuf::from(crate::config::DEFAULT_INSTALL_DIR),
    };
    write_config(&cfg, &opts.config_path).map_err(|source| EnrollCmdError::WriteConfig {
        path: opts.config_path.to_path_buf(),
        source,
    })?;
    Ok(cfg)
}

/// Write agent.toml: 0600, temp + fsync + rename in the destination
/// directory (D4 step 3; Law 3 atomic writes).
pub fn write_config(cfg: &AgentConfig, path: &Path) -> std::io::Result<()> {
    use std::io::Write as _;

    let toml = toml::to_string_pretty(cfg).map_err(std::io::Error::other)?;
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(dir) = dir {
        std::fs::create_dir_all(dir)?;
    }

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("agent.toml");
    let tmp = match dir {
        Some(d) => d.join(format!(".{file_name}.tmp")),
        None => PathBuf::from(format!(".{file_name}.tmp")),
    };

    let mut open = std::fs::OpenOptions::new();
    open.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // The token is a credential: owner-only from the first byte.
        open.mode(0o600);
    }
    let mut f = open.open(&tmp)?;
    f.write_all(toml.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)?;
    // fsync the directory so the rename itself is durable.
    if let Some(dir) = dir
        && let Ok(d) = std::fs::File::open(dir)
    {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Usage string for `reeve-agent enroll`.
pub const ENROLL_USAGE: &str = "usage: reeve-agent enroll --server <URL> --token <JOIN_TOKEN> \
     [--config <PATH>] [--data-dir <PATH>]";

/// Parse `reeve-agent enroll` arguments (everything after the
/// subcommand). Deliberately dependency-free — two required flags and
/// two optional ones do not justify a CLI framework.
///
/// `--config` defaults to `$REEVE_AGENT_CONFIG` or
/// [`crate::config::DEFAULT_CONFIG_PATH`] — the same resolution the
/// daemon uses to load it (D4 step 3).
pub fn parse_enroll_args<I: IntoIterator<Item = String>>(args: I) -> Result<EnrollOpts, String> {
    let mut server = None;
    let mut token = None;
    let mut config_path = None;
    let mut data_dir = None;

    let mut it = args.into_iter();
    while let Some(flag) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .ok_or_else(|| format!("{name} requires a value\n{ENROLL_USAGE}"))
        };
        match flag.as_str() {
            "--server" => server = Some(value("--server")?),
            "--token" => token = Some(value("--token")?),
            "--config" => config_path = Some(PathBuf::from(value("--config")?)),
            "--data-dir" => data_dir = Some(PathBuf::from(value("--data-dir")?)),
            other => return Err(format!("unknown argument {other:?}\n{ENROLL_USAGE}")),
        }
    }

    Ok(EnrollOpts {
        server: server.ok_or_else(|| format!("--server is required\n{ENROLL_USAGE}"))?,
        join_token: token.ok_or_else(|| format!("--token is required\n{ENROLL_USAGE}"))?,
        config_path: config_path.unwrap_or_else(|| {
            std::env::var_os(crate::config::CONFIG_PATH_ENV)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(crate::config::DEFAULT_CONFIG_PATH))
        }),
        data_dir,
    })
}

/// Best-effort hostname detection without a platform dependency:
/// kernel, then /etc/hostname, then $HOSTNAME, then "unknown".
pub fn detect_hostname() -> String {
    for path in ["/proc/sys/kernel/hostname", "/etc/hostname"] {
        if let Ok(s) = std::fs::read_to_string(path) {
            let s = s.trim();
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    std::env::var("HOSTNAME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;
    use axum::{Json, Router};

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn enroll_writes_config_0600_atomically() {
        let app = Router::new().route(
            ENROLL_PATH,
            post(|Json(req): Json<EnrollRequest>| async move {
                assert!(!req.hostname.is_empty(), "hostname must be gathered");
                assert!(!req.arch.is_empty());
                assert!(!req.agent_version.is_empty());
                assert_eq!(req.join_token, "rvj_good");
                Json(EnrollResponse {
                    device_id: "dev-42".into(),
                    device_token: "rvd_tok".into(),
                    resumed: false,
                })
            }),
        );
        let base = serve(app).await;

        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("etc").join("agent.toml");
        let opts = EnrollOpts {
            // trailing slash must be tolerated
            server: format!("{base}/"),
            join_token: "rvj_good".into(),
            config_path: config_path.clone(),
            data_dir: Some(dir.path().join("data")),
        };
        let cfg = enroll(&opts).await.unwrap();
        assert_eq!(cfg.device_id.as_deref(), Some("dev-42"));

        // the file round-trips through the normal loader
        let loaded = AgentConfig::from_path(&config_path).unwrap();
        assert_eq!(loaded, cfg);
        assert_eq!(loaded.server, base, "trailing slash trimmed");
        assert_eq!(loaded.device_token.as_deref(), Some("rvd_tok"));

        // 0600 (D4 step 3)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&config_path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "agent.toml must be 0600");
        }

        // no temp file left behind
        let leftovers: Vec<_> = std::fs::read_dir(config_path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("agent.toml")]);
    }

    #[tokio::test]
    async fn rejected_token_is_a_clean_error() {
        let app = Router::new().route(
            ENROLL_PATH,
            post(|| async {
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({"error": "invalid, expired, or exhausted join token"})),
                )
            }),
        );
        let base = serve(app).await;

        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("agent.toml");
        let err = enroll(&EnrollOpts {
            server: base,
            join_token: "rvj_bad".into(),
            config_path: config_path.clone(),
            data_dir: None,
        })
        .await
        .unwrap_err();
        assert!(matches!(err, EnrollCmdError::TokenRejected));
        assert!(!config_path.exists(), "no config written on failure");
    }

    #[tokio::test]
    async fn unreachable_server_is_a_clean_error() {
        // reserved TEST-NET-1 address: nothing listens there
        let err = enroll(&EnrollOpts {
            server: "http://127.0.0.1:1".into(),
            join_token: "rvj_x".into(),
            config_path: PathBuf::from("/nonexistent/agent.toml"),
            data_dir: None,
        })
        .await
        .unwrap_err();
        assert!(matches!(err, EnrollCmdError::Unreachable { .. }));
    }

    #[test]
    fn parse_enroll_args_full_and_minimal() {
        let sv = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        let opts = parse_enroll_args(sv(&[
            "--server",
            "https://reeve.example",
            "--token",
            "rvj_x",
            "--config",
            "/tmp/a.toml",
            "--data-dir",
            "/tmp/data",
        ]))
        .unwrap();
        assert_eq!(opts.server, "https://reeve.example");
        assert_eq!(opts.join_token, "rvj_x");
        assert_eq!(opts.config_path, PathBuf::from("/tmp/a.toml"));
        assert_eq!(opts.data_dir, Some(PathBuf::from("/tmp/data")));

        assert!(parse_enroll_args(sv(&["--server", "https://x"]))
            .unwrap_err()
            .contains("--token is required"));
        assert!(parse_enroll_args(sv(&["--bogus"]))
            .unwrap_err()
            .contains("unknown argument"));
        assert!(parse_enroll_args(sv(&["--server"]))
            .unwrap_err()
            .contains("requires a value"));
    }

    #[test]
    fn detect_hostname_never_empty() {
        assert!(!detect_hostname().is_empty());
    }

    #[test]
    fn write_config_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.toml");
        let mk = |token: &str| AgentConfig {
            server: "https://x".into(),
            device_token: Some(token.into()),
            device_id: Some("dev-1".into()),
            poll_interval_secs: DEFAULT_POLL_INTERVAL_SECS,
            data_dir: PathBuf::from(DEFAULT_DATA_DIR),
            health_interval_secs: crate::config::DEFAULT_HEALTH_INTERVAL_SECS,
            journal_retention_days: crate::config::DEFAULT_JOURNAL_RETENTION_DAYS,
            journal_max_bytes: crate::config::DEFAULT_JOURNAL_MAX_BYTES,
            install_dir: PathBuf::from(crate::config::DEFAULT_INSTALL_DIR),
        };
        write_config(&mk("a"), &path).unwrap();
        write_config(&mk("b"), &path).unwrap();
        let loaded = AgentConfig::from_path(&path).unwrap();
        assert_eq!(loaded.device_token.as_deref(), Some("b"));
    }
}

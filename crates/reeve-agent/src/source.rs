//! Manifest source abstraction — where desired state comes from.
//!
//! Two first-class schemes, ONE code path above this module
//! (CLAUDE.md Build order, Milestone 1):
//! - `https://…` (or `http://` for tests): a reeve server's
//!   `GET /api/reeve/v1/manifest` with the device bearer token and
//!   conditional GET (`If-None-Match` = manifest digest ETag,
//!   RFC 9110 strong validator, grammar `sha256:<hex>`;
//!   spec/reeve/08-packaging.md §10.2; docs/decisions/delivery.md D13).
//! - `dir://<path>`: a local directory holding `manifest.json` (+
//!   an OCI layout for the bundle, consumed by the fetch step, B2).
//!   Same conditional semantics: the ETag is the sha256 digest of
//!   the manifest bytes; an unchanged digest is a 304-equivalent.
//!   This is the Milestone 1 harness AND the air-gap media apply
//!   path — deliberately the same code.
//!
//! Every failure is classified so the poll loop can apply Law 5:
//! offline/unreachable is a logged no-op, never an error that stops
//! the agent.

use std::path::{Path, PathBuf};
use std::time::Duration;

use reeve_types::reeve::capabilities::ServerCapabilities;
use reeve_types::reeve::manifest::StateManifest;
use sha2::{Digest, Sha256};

/// Manifest endpoint path on a reeve server
/// (spec/reeve/08-packaging.md §10.2).
pub const MANIFEST_PATH: &str = "/api/reeve/v1/manifest";
/// Capability endpoint path (spec/reeve/01-framework.md §3.3).
pub const CAPABILITIES_PATH: &str = "/api/reeve/v1/capabilities";
/// Manifest file name inside a `dir://` source.
pub const DIR_MANIFEST_FILE: &str = "manifest.json";

/// Why a poll did not produce a manifest.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// Couldn't reach the source at all (network down, DNS, timeout,
    /// media unmounted). Law 5: caller continues from last known
    /// state — this is expected operation, not a fault.
    #[error("source unreachable: {0}")]
    Unreachable(String),
    /// Reached the source but the exchange was invalid (bad status,
    /// unparseable body). Also a continue-from-last-known-state
    /// path, but logged at error severity.
    #[error("protocol error: {0}")]
    Protocol(String),
}

/// A successful poll.
#[derive(Debug, Clone, PartialEq)]
pub enum PollResponse {
    /// `If-None-Match` matched (HTTP 304 / unchanged dir digest).
    NotModified,
    /// A (possibly new) manifest, with its ETag.
    Manifest {
        manifest: StateManifest,
        /// Strong validator, digest grammar `sha256:<hex>`
        /// (spec/reeve/08-packaging.md §10.2).
        etag: String,
    },
}

/// A parsed manifest source.
pub enum ManifestSource {
    Http(HttpSource),
    Dir(DirSource),
}

/// Error parsing a source URL.
#[derive(Debug, thiserror::Error)]
#[error("unsupported manifest source {url:?}: expected https://, http://, or dir://")]
pub struct ParseSourceError {
    pub url: String,
}

impl ManifestSource {
    /// Parse `https://…` / `http://…` / `dir://<path>` into a
    /// source. `device_token` applies to HTTP sources only
    /// (docs/decisions/agent.md D4: the one device credential).
    pub fn parse(url: &str, device_token: Option<String>) -> Result<Self, ParseSourceError> {
        if let Some(path) = url.strip_prefix("dir://") {
            return Ok(ManifestSource::Dir(DirSource {
                dir: PathBuf::from(path),
            }));
        }
        if url.starts_with("https://") || url.starts_with("http://") {
            return Ok(ManifestSource::Http(HttpSource::new(url, device_token)));
        }
        Err(ParseSourceError { url: url.to_string() })
    }

    /// Poll for the manifest, conditionally on `if_none_match`.
    pub async fn poll_manifest(
        &self,
        if_none_match: Option<&str>,
    ) -> Result<PollResponse, SourceError> {
        match self {
            ManifestSource::Http(s) => s.poll_manifest(if_none_match).await,
            ManifestSource::Dir(s) => s.poll_manifest(if_none_match),
        }
    }

    /// Probe server capabilities (spec/reeve/01-framework.md §3.3).
    /// `None` means "vanilla Margo server" — 404 OR ANY ERROR maps
    /// here, and the agent MUST proceed with pure Margo behavior
    /// (§3.2 degradation; probing must never break convergence,
    /// Law 5). `dir://` sources have no server: always `None`.
    pub async fn probe_capabilities(&self) -> Option<ServerCapabilities> {
        match self {
            ManifestSource::Http(s) => s.probe_capabilities().await,
            ManifestSource::Dir(_) => None,
        }
    }
}

/// `sha256:<hex>` digest of raw bytes — the ETag a `dir://` source
/// computes, and the fallback when an HTTP response omits ETag.
pub fn digest_bytes(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("sha256:{:x}", h.finalize())
}

/// HTTP(S) manifest source.
pub struct HttpSource {
    base: String,
    device_token: Option<String>,
    client: reqwest::Client,
}

impl HttpSource {
    fn new(base: &str, device_token: Option<String>) -> Self {
        HttpSource {
            base: base.trim_end_matches('/').to_string(),
            device_token,
            // Offline-first: never hang a poll; a slow WAN link is
            // indistinguishable from offline past this budget.
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("static reqwest client config"),
        }
    }

    fn authorize(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.device_token {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }

    async fn poll_manifest(&self, if_none_match: Option<&str>) -> Result<PollResponse, SourceError> {
        let mut req = self.authorize(self.client.get(format!("{}{MANIFEST_PATH}", self.base)));
        if let Some(etag) = if_none_match {
            // The ETag is a strong validator; send it verbatim,
            // quoted per RFC 9110 field syntax.
            req = req.header("If-None-Match", format!("\"{etag}\""));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| SourceError::Unreachable(e.to_string()))?;
        match resp.status().as_u16() {
            304 => Ok(PollResponse::NotModified),
            200 => {
                let etag_header = resp
                    .headers()
                    .get("etag")
                    .and_then(|v| v.to_str().ok())
                    .map(|v| v.trim_matches('"').to_string());
                let body = resp
                    .bytes()
                    .await
                    .map_err(|e| SourceError::Unreachable(e.to_string()))?;
                let manifest: StateManifest = serde_json::from_slice(&body)
                    .map_err(|e| SourceError::Protocol(format!("bad manifest body: {e}")))?;
                // Fallback keeps conditional GET working against a
                // server that forgot the header.
                let etag = etag_header.unwrap_or_else(|| digest_bytes(&body));
                Ok(PollResponse::Manifest { manifest, etag })
            }
            s => Err(SourceError::Protocol(format!(
                "unexpected status {s} from {MANIFEST_PATH}"
            ))),
        }
    }

    async fn probe_capabilities(&self) -> Option<ServerCapabilities> {
        let resp = self
            .authorize(self.client.get(format!("{}{CAPABILITIES_PATH}", self.base)))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            // 404 = vanilla Margo server (§3.3); any other status
            // degrades the same way (§3.2).
            return None;
        }
        let body = resp.bytes().await.ok()?;
        serde_json::from_slice::<ServerCapabilities>(&body).ok()
    }
}

/// `dir://` manifest source — Milestone 1 harness and air-gap media.
pub struct DirSource {
    dir: PathBuf,
}

impl DirSource {
    /// The directory this source reads from.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn poll_manifest(&self, if_none_match: Option<&str>) -> Result<PollResponse, SourceError> {
        let path = self.dir.join(DIR_MANIFEST_FILE);
        let bytes = std::fs::read(&path).map_err(|e| {
            // Missing media/dir is the offline analog (Law 5).
            SourceError::Unreachable(format!("cannot read {}: {e}", path.display()))
        })?;
        let etag = digest_bytes(&bytes);
        if if_none_match == Some(etag.as_str()) {
            return Ok(PollResponse::NotModified);
        }
        let manifest: StateManifest = serde_json::from_slice(&bytes)
            .map_err(|e| SourceError::Protocol(format!("bad {DIR_MANIFEST_FILE}: {e}")))?;
        Ok(PollResponse::Manifest { manifest, etag })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reeve_types::reeve::manifest::ManifestVersion;

    #[test]
    fn parse_schemes() {
        assert!(matches!(
            ManifestSource::parse("dir:///opt/src", None).unwrap(),
            ManifestSource::Dir(_)
        ));
        assert!(matches!(
            ManifestSource::parse("https://reeve.example", Some("t".into())).unwrap(),
            ManifestSource::Http(_)
        ));
        assert!(matches!(
            ManifestSource::parse("http://127.0.0.1:8080/", None).unwrap(),
            ManifestSource::Http(_)
        ));
        assert!(ManifestSource::parse("oci://reg/x", None).is_err());
        assert!(ManifestSource::parse("/bare/path", None).is_err());
    }

    #[test]
    fn dir_parse_keeps_path() {
        let ManifestSource::Dir(d) = ManifestSource::parse("dir:///opt/src", None).unwrap() else {
            panic!("expected dir source");
        };
        assert_eq!(d.dir(), Path::new("/opt/src"));
    }

    fn write_manifest(dir: &Path, version: u64) -> String {
        let body = serde_json::json!({
            "manifestVersion": version,
            "bundle": {
                "digest": format!("sha256:{}", "b".repeat(64)),
                "url": "oci-layout/",
            },
        })
        .to_string();
        std::fs::write(dir.join(DIR_MANIFEST_FILE), &body).unwrap();
        digest_bytes(body.as_bytes())
    }

    #[tokio::test]
    async fn dir_source_conditional_poll() {
        let tmp = tempfile::tempdir().unwrap();
        let etag = write_manifest(tmp.path(), 1);
        let src = ManifestSource::parse(&format!("dir://{}", tmp.path().display()), None).unwrap();

        // First poll: manifest + digest etag.
        let got = src.poll_manifest(None).await.unwrap();
        let PollResponse::Manifest { manifest, etag: got_etag } = got else {
            panic!("expected manifest");
        };
        assert_eq!(manifest.manifest_version, ManifestVersion(1));
        assert_eq!(got_etag, etag);

        // Unchanged: 304-equivalent.
        assert_eq!(
            src.poll_manifest(Some(&etag)).await.unwrap(),
            PollResponse::NotModified
        );

        // Changed file: new manifest, new etag.
        let etag2 = write_manifest(tmp.path(), 2);
        let got = src.poll_manifest(Some(&etag)).await.unwrap();
        let PollResponse::Manifest { etag: got_etag2, .. } = got else {
            panic!("expected manifest");
        };
        assert_eq!(got_etag2, etag2);
        assert_ne!(etag, etag2);
    }

    #[tokio::test]
    async fn dir_source_missing_is_unreachable() {
        let src = ManifestSource::parse("dir:///nonexistent-reeve-test", None).unwrap();
        assert!(matches!(
            src.poll_manifest(None).await,
            Err(SourceError::Unreachable(_))
        ));
    }

    #[tokio::test]
    async fn dir_source_garbage_is_protocol_error() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(DIR_MANIFEST_FILE), b"not json").unwrap();
        let src = ManifestSource::parse(&format!("dir://{}", tmp.path().display()), None).unwrap();
        assert!(matches!(
            src.poll_manifest(None).await,
            Err(SourceError::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn dir_source_has_no_capabilities() {
        let tmp = tempfile::tempdir().unwrap();
        write_manifest(tmp.path(), 1);
        let src = ManifestSource::parse(&format!("dir://{}", tmp.path().display()), None).unwrap();
        assert!(src.probe_capabilities().await.is_none());
    }

    #[test]
    fn digest_matches_grammar() {
        let d = digest_bytes(b"hello");
        assert!(reeve_types::reeve::manifest::is_sha256_digest(&d));
    }
}

//! Render-bundle pull — content-addressed OCI pull, verify, unpack,
//! atomic swap (build item B2; core, unconditional).
//!
//! Normative sources:
//! - spec/reeve/08-packaging.md §10.2 "Artifact pull": the render
//!   bundle is an OCI artifact served read-only on standard /v2
//!   distribution routes (GET manifest / GET blob by digest); pull is
//!   content-addressed and immutable: verify digest, unpack to temp,
//!   atomic dir swap, converge.
//! - docs/decisions/delivery.md D7 (native read-only OCI serving,
//!   device token auth) and D13 (bundle digest in the State
//!   Manifest; devices never speak git).
//! - docs/decisions/tree-render.md D2 (bundle layout: manifest.yaml +
//!   apps/<name>/…; agent-local dirs work/ and applied/; "applied
//!   bundle digest recorded in agent.db, not a loose file").
//!
//! Crash-only mechanics (Law 3 — `kill -9` at any byte leaves either
//! old state or new state COMPLETE):
//! - Bundles unpack into `work/` (temp), are validated + fsynced,
//!   then renamed to the content-addressed dir
//!   `bundles/<hex>` — presence in `bundles/` therefore ALWAYS means
//!   "complete and digest-verified".
//! - The current bundle is the symlink `bundle -> bundles/<hex>`;
//!   the swap is one atomic `rename(2)` of a pre-made symlink. A
//!   crash before the rename leaves the old target; after, the new.
//! - agent.db `bundle_state` is written ONLY after the swap. Startup
//!   recovery ([`BundleStore::recover`]) wipes `work/`, rolls the DB
//!   forward if the swap landed but the record didn't, and GCs
//!   unreferenced bundle dirs.
//!
//! Offline-first (Law 5): every fetch failure is classified and the
//! caller continues from the last swapped bundle; a failed pull
//! never disturbs current state.

use std::fs::{self, File};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use flate2::read::GzDecoder;
use reeve_types::reeve::manifest::{BundleRef, RENDER_BUNDLE_MEDIA_TYPE, is_sha256_digest};
use serde::Deserialize;
use tracing::{info, warn};

use crate::source::{ParseSourceError, digest_bytes};
use crate::state::{AgentDb, Severity, StateError};

/// OCI image manifest media type (image-spec v1) — what the /v2
/// manifest GET returns for a render-bundle artifact
/// (spec/reeve/08-packaging.md §10.2: standard OCI distribution pull;
/// stock clients must work, so we speak stock shapes).
pub const OCI_MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

/// Name of the current-bundle symlink inside the agent data dir.
pub const BUNDLE_LINK: &str = "bundle";
/// Content-addressed complete-bundle directory (dir name = digest
/// hex; presence == complete, see module docs).
pub const BUNDLES_DIR: &str = "bundles";
/// Temp dir for downloads and partial unpacks — wiped at startup
/// (docs/decisions/tree-render.md D2 `work/`).
pub const WORK_DIR: &str = "work";

/// Why a bundle pull failed. Every variant is a
/// continue-from-last-known-state path for the caller (Law 5); none
/// of them can have disturbed the currently swapped bundle.
#[derive(Debug, thiserror::Error)]
pub enum PullError {
    /// Couldn't reach the artifact source (network down, media
    /// unmounted). Expected operation for an offline-first agent.
    #[error("bundle source unreachable: {0}")]
    Unreachable(String),
    /// Reached the source but the exchange was invalid (bad status,
    /// unparseable OCI manifest, missing blob in a present layout).
    #[error("bundle protocol error: {0}")]
    Protocol(String),
    /// Fetched bytes do not hash to the digest that named them —
    /// fail closed, nothing is unpacked (§10.2: verify digest).
    #[error("digest mismatch for {what}: expected {expected}, got {actual}")]
    DigestMismatch {
        what: &'static str,
        expected: String,
        actual: String,
    },
    /// The verified bundle violates the D2 layout contract (missing
    /// manifest.yaml, traversal paths, non-file/dir tar entries).
    #[error("bad bundle layout: {0}")]
    BadLayout(String),
    /// Local filesystem failure in the store.
    #[error("bundle store io: {0}")]
    Io(#[from] std::io::Error),
    /// agent.db failure recording/reading bundle state.
    #[error(transparent)]
    State(#[from] StateError),
}

/// Minimal OCI image-manifest shape — just enough to select the
/// render-bundle layer. Unknown fields tolerated (stock registries
/// add annotations, artifactType, subject, …).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OciManifest {
    #[serde(default)]
    pub media_type: Option<String>,
    #[serde(default)]
    pub layers: Vec<OciDescriptor>,
}

/// OCI content descriptor (image-spec v1 `descriptor.md`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OciDescriptor {
    #[serde(default)]
    pub media_type: Option<String>,
    pub digest: String,
    #[serde(default)]
    pub size: Option<u64>,
}

/// Where render-bundle artifacts come from. Parses the SAME source
/// URL as [`crate::ManifestSource`] (one `server =` config value,
/// docs/decisions/agent.md D4: one credential, one endpoint):
/// - `https://…` / `http://…`: standard OCI distribution pull under
///   the server's /v2 routes with the device bearer token (D7).
/// - `dir://<path>`: bundle URLs resolve relative to the manifest
///   dir and point at an OCI **layout** directory
///   (`blobs/sha256/<hex>`) — the Milestone 1 harness and the
///   air-gap media apply path, deliberately the same code.
pub enum BundleSource {
    Http(HttpBundleSource),
    Dir(DirBundleSource),
}

impl BundleSource {
    /// Parse the agent's `server` URL into a bundle source.
    pub fn parse(url: &str, device_token: Option<String>) -> Result<Self, ParseSourceError> {
        if let Some(path) = url.strip_prefix("dir://") {
            return Ok(BundleSource::Dir(DirBundleSource {
                dir: PathBuf::from(path),
            }));
        }
        if url.starts_with("https://") || url.starts_with("http://") {
            return Ok(BundleSource::Http(HttpBundleSource::new(url, device_token)));
        }
        Err(ParseSourceError {
            url: url.to_string(),
        })
    }

    /// Fetch and VERIFY the render-bundle layer named by `bundle`:
    /// GET the OCI manifest by `bundle.digest`, verify its bytes
    /// hash to that digest, select the single layer with media type
    /// [`RENDER_BUNDLE_MEDIA_TYPE`], GET that blob, verify its
    /// digest, return the verified tar.gz bytes
    /// (spec/reeve/08-packaging.md §10.2). `sizeBytes` is advisory
    /// only — the digest is the sole integrity check
    /// (reeve-types BundleRef doc).
    pub async fn fetch(&self, bundle: &BundleRef) -> Result<Vec<u8>, PullError> {
        if !is_sha256_digest(&bundle.digest) {
            return Err(PullError::Protocol(format!(
                "bundle digest {:?} violates sha256:<hex> grammar",
                bundle.digest
            )));
        }
        let manifest_bytes = match self {
            BundleSource::Http(s) => s.fetch_manifest(&bundle.url, &bundle.digest).await?,
            BundleSource::Dir(s) => s.read_blob(&bundle.url, &bundle.digest)?,
        };
        verify_digest(&manifest_bytes, &bundle.digest, "OCI manifest")?;
        let manifest: OciManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| PullError::Protocol(format!("unparseable OCI manifest: {e}")))?;
        if let Some(mt) = &manifest.media_type
            && mt != OCI_MANIFEST_MEDIA_TYPE
        {
            return Err(PullError::Protocol(format!(
                "unexpected OCI manifest mediaType {mt:?}"
            )));
        }
        let mut render_layers = manifest
            .layers
            .iter()
            .filter(|l| l.media_type.as_deref() == Some(RENDER_BUNDLE_MEDIA_TYPE));
        let layer = match (render_layers.next(), render_layers.next()) {
            (Some(l), None) => l,
            (None, _) => {
                return Err(PullError::Protocol(format!(
                    "no layer with media type {RENDER_BUNDLE_MEDIA_TYPE} in OCI manifest"
                )));
            }
            (Some(_), Some(_)) => {
                return Err(PullError::Protocol(format!(
                    "multiple {RENDER_BUNDLE_MEDIA_TYPE} layers in OCI manifest"
                )));
            }
        };
        if !is_sha256_digest(&layer.digest) {
            return Err(PullError::Protocol(format!(
                "layer digest {:?} violates sha256:<hex> grammar",
                layer.digest
            )));
        }
        let blob = match self {
            BundleSource::Http(s) => s.fetch_blob(&bundle.url, &layer.digest).await?,
            BundleSource::Dir(s) => s.read_blob(&bundle.url, &layer.digest)?,
        };
        verify_digest(&blob, &layer.digest, "render-bundle layer")?;
        Ok(blob)
    }
}

fn verify_digest(bytes: &[u8], expected: &str, what: &'static str) -> Result<(), PullError> {
    let actual = digest_bytes(bytes);
    if actual != expected {
        return Err(PullError::DigestMismatch {
            what,
            expected: expected.to_string(),
            actual,
        });
    }
    Ok(())
}

/// HTTP(S) OCI distribution pull client — GET manifest / GET blob by
/// digest, device bearer token (docs/decisions/delivery.md D7;
/// spec/reeve/08-packaging.md §10.2). Minimal by design: no tags, no
/// push, no token dance — reeve's /v2 uses the same bearer scheme as
/// everything else.
pub struct HttpBundleSource {
    origin: String,
    device_token: Option<String>,
    client: reqwest::Client,
}

impl HttpBundleSource {
    fn new(origin: &str, device_token: Option<String>) -> Self {
        HttpBundleSource {
            origin: origin.trim_end_matches('/').to_string(),
            device_token,
            // Bundles are config-scale; a stalled WAN transfer is
            // offline past this budget (Law 5).
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .expect("static reqwest client config"),
        }
    }

    /// Resolve the manifest's `bundle.url` to the repository base
    /// URL: absolute URLs pass through; server-relative paths
    /// (`/v2/<name>`) join the configured origin; a full
    /// `…/manifests/<digest>` URL is trimmed to its repo base so
    /// both URL styles work.
    fn repo_base(&self, url: &str) -> String {
        let abs = if url.starts_with("https://") || url.starts_with("http://") {
            url.to_string()
        } else if let Some(rest) = url.strip_prefix('/') {
            format!("{}/{rest}", self.origin)
        } else {
            format!("{}/{url}", self.origin)
        };
        let abs = abs.trim_end_matches('/');
        match abs.find("/manifests/") {
            Some(i) => abs[..i].to_string(),
            None => abs.to_string(),
        }
    }

    fn authorize(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.device_token {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }

    async fn get_verified_bytes(&self, url: String) -> Result<Vec<u8>, PullError> {
        let resp = self
            .authorize(self.client.get(&url))
            .header("Accept", OCI_MANIFEST_MEDIA_TYPE)
            .send()
            .await
            .map_err(|e| PullError::Unreachable(e.to_string()))?;
        let status = resp.status().as_u16();
        if status != 200 {
            return Err(PullError::Protocol(format!(
                "unexpected status {status} from {url}"
            )));
        }
        let body = resp
            .bytes()
            .await
            .map_err(|e| PullError::Unreachable(e.to_string()))?;
        Ok(body.to_vec())
    }

    /// `GET {repo}/manifests/{digest}` (OCI distribution spec).
    async fn fetch_manifest(&self, url: &str, digest: &str) -> Result<Vec<u8>, PullError> {
        self.get_verified_bytes(format!("{}/manifests/{digest}", self.repo_base(url)))
            .await
    }

    /// `GET {repo}/blobs/{digest}` (OCI distribution spec).
    async fn fetch_blob(&self, url: &str, digest: &str) -> Result<Vec<u8>, PullError> {
        self.get_verified_bytes(format!("{}/blobs/{digest}", self.repo_base(url)))
            .await
    }
}

/// `dir://` bundle source — an OCI **layout** directory
/// (image-spec `image-layout.md`: `blobs/sha256/<hex>` files).
/// Milestone 1 harness and air-gap media apply (CLAUDE.md Build
/// order): `oras copy --to-oci-layout` output is directly consumable.
pub struct DirBundleSource {
    dir: PathBuf,
}

impl DirBundleSource {
    /// Resolve `bundle.url` to the OCI layout root: absolute paths
    /// and `dir://` URLs pass through; relative paths resolve
    /// against the manifest source dir.
    fn layout_root(&self, url: &str) -> PathBuf {
        let p = url.strip_prefix("dir://").unwrap_or(url);
        let path = Path::new(p);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.dir.join(path)
        }
    }

    /// Read one blob by digest from `blobs/sha256/<hex>`. A missing
    /// layout root is the offline analog (media unmounted, Law 5); a
    /// missing blob inside a present layout is a protocol error.
    fn read_blob(&self, url: &str, digest: &str) -> Result<Vec<u8>, PullError> {
        let root = self.layout_root(url);
        if !root.is_dir() {
            return Err(PullError::Unreachable(format!(
                "OCI layout dir {} not present",
                root.display()
            )));
        }
        let hex = digest
            .strip_prefix("sha256:")
            .expect("digest grammar validated by caller");
        let path = root.join("blobs").join("sha256").join(hex);
        fs::read(&path).map_err(|e| {
            PullError::Protocol(format!("cannot read blob {}: {e}", path.display()))
        })
    }
}

/// The agent's on-disk bundle store under `data_dir` — owns
/// `bundles/`, `work/`, and the `bundle` symlink (see module docs
/// for the crash-only invariants).
pub struct BundleStore {
    data_dir: PathBuf,
    bundles_dir: PathBuf,
    work_dir: PathBuf,
    link_path: PathBuf,
}

impl BundleStore {
    /// Open (creating if absent) the store directories. Idempotent —
    /// part of startup recovery (Law 3). Call
    /// [`BundleStore::recover`] next.
    pub fn open(data_dir: &Path) -> Result<Self, PullError> {
        let store = BundleStore {
            data_dir: data_dir.to_path_buf(),
            bundles_dir: data_dir.join(BUNDLES_DIR),
            work_dir: data_dir.join(WORK_DIR),
            link_path: data_dir.join(BUNDLE_LINK),
        };
        fs::create_dir_all(&store.bundles_dir)?;
        fs::create_dir_all(&store.work_dir)?;
        Ok(store)
    }

    /// Path of the current-bundle symlink (`<data_dir>/bundle`) —
    /// what converge (B3) reads through.
    pub fn current_path(&self) -> &Path {
        &self.link_path
    }

    /// Digest (`sha256:<hex>`) of the currently swapped bundle,
    /// derived from DISK (the symlink target's name) — self-
    /// describing, no DB read. `None` if no complete bundle is
    /// swapped in.
    pub fn current_digest(&self) -> Option<String> {
        self.link_target_hex().map(|hex| format!("sha256:{hex}"))
    }

    fn link_target_hex(&self) -> Option<String> {
        let target = fs::read_link(&self.link_path).ok()?;
        let hex = target.file_name()?.to_str()?.to_string();
        if self.bundles_dir.join(&hex).is_dir() {
            Some(hex)
        } else {
            None
        }
    }

    /// Startup recovery (Law 3: startup IS recovery). Idempotent:
    /// - wipes `work/` — any content there is a crashed partial
    ///   download/unpack, safe to discard because completeness lives
    ///   only in `bundles/`;
    /// - rolls the DB forward if a `kill -9` landed between the
    ///   atomic swap and the `bundle_state` record (the swap is the
    ///   commitment point; disk is truth);
    /// - GCs `bundles/` dirs the current symlink doesn't reference.
    ///
    /// Returns the digest of the bundle in place, if any.
    pub fn recover(&self, db: &mut AgentDb) -> Result<Option<String>, PullError> {
        for entry in fs::read_dir(&self.work_dir)? {
            let path = entry?.path();
            let removed = if path.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            if let Err(e) = removed {
                warn!(path = %path.display(), error = %e, "could not clean work entry");
            } else {
                info!(path = %path.display(), "cleaned crashed work entry");
            }
        }

        // A dangling link (target GC'd out from under it — external
        // interference only) reads as "no bundle"; remove it so the
        // path never resolves to garbage.
        if fs::symlink_metadata(&self.link_path).is_ok() && self.link_target_hex().is_none() {
            fs::remove_file(&self.link_path)?;
        }

        let disk = self.current_digest();
        let recorded = db.pulled_bundle()?;
        if recorded != disk {
            match &disk {
                Some(d) => {
                    // Swap landed, record didn't: complete the
                    // interrupted operation forward.
                    db.record_bundle(
                        d,
                        "bundle-rolled-forward",
                        &format!(
                            "startup found swapped bundle {d} unrecorded (was {recorded:?})"
                        ),
                    )?;
                }
                None => {
                    db.clear_bundle(&format!(
                        "recorded bundle {recorded:?} not on disk; continuing without a bundle"
                    ))?;
                }
            }
        }

        self.gc(disk.as_deref().map(|d| &d[7..]));
        Ok(disk)
    }

    /// Remove complete-but-unreferenced bundle dirs. Best-effort —
    /// a failed GC only costs disk, never correctness.
    fn gc(&self, keep_hex: Option<&str>) {
        let Ok(entries) = fs::read_dir(&self.bundles_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if keep_hex.is_some_and(|k| name.to_str() == Some(k)) {
                continue;
            }
            if let Err(e) = fs::remove_dir_all(entry.path()) {
                warn!(path = %entry.path().display(), error = %e, "bundle gc failed");
            }
        }
    }

    /// Ensure the bundle referenced by the last ACCEPTED manifest is
    /// pulled, verified, and swapped in. The resume entry point:
    /// call after every poll (including 304 — an accept whose pull
    /// crashed or failed must retry) and at startup. `Ok(None)` when
    /// there is no accepted manifest or it carries no bundle
    /// (`bundle: null`, zero apps — removal convergence is B3's).
    pub async fn sync(
        &self,
        db: &mut AgentDb,
        source: &BundleSource,
    ) -> Result<Option<PathBuf>, PullError> {
        let Some(accepted) = db.last_accepted()? else {
            return Ok(None);
        };
        let Some(bundle) = accepted.manifest.bundle else {
            return Ok(None);
        };
        self.apply(db, source, &bundle).await.map(Some)
    }

    /// Pull + verify + unpack + atomically swap one bundle into
    /// place, recording the digest in agent.db ONLY after the swap
    /// (spec/reeve/08-packaging.md §10.2: verify digest, unpack to
    /// temp, atomic dir swap). Idempotent at every stage:
    /// - digest already swapped and recorded => no-op, no fetch;
    /// - complete dir already in `bundles/` (crash after rename,
    ///   before swap) => resume at the swap, no re-fetch;
    /// - anything less => full pull into `work/`, then
    ///   rename -> swap -> record.
    ///
    /// Returns the current-bundle path ([`Self::current_path`]).
    /// On ANY error the previously swapped bundle is untouched.
    pub async fn apply(
        &self,
        db: &mut AgentDb,
        source: &BundleSource,
        bundle: &BundleRef,
    ) -> Result<PathBuf, PullError> {
        let result = self.apply_inner(db, source, bundle).await;
        if let Err(e) = &result {
            // Journal the failure class (best-effort; the error is
            // also returned). Unreachable is expected operation for
            // an offline-first agent — info, not error (Law 5).
            let (severity, event) = match e {
                PullError::Unreachable(_) => (Severity::Info, "bundle-pull-failed"),
                PullError::DigestMismatch { .. } => (Severity::Error, "bundle-bad-digest"),
                PullError::BadLayout(_) => (Severity::Error, "bundle-bad-layout"),
                PullError::Protocol(_) | PullError::Io(_) | PullError::State(_) => {
                    (Severity::Error, "bundle-pull-failed")
                }
            };
            let _ = db.journal(severity, event, &e.to_string());
        }
        result
    }

    async fn apply_inner(
        &self,
        db: &mut AgentDb,
        source: &BundleSource,
        bundle: &BundleRef,
    ) -> Result<PathBuf, PullError> {
        if !is_sha256_digest(&bundle.digest) {
            return Err(PullError::Protocol(format!(
                "bundle digest {:?} violates sha256:<hex> grammar",
                bundle.digest
            )));
        }
        let hex = bundle.digest["sha256:".len()..].to_string();
        let final_dir = self.bundles_dir.join(&hex);

        // Fully done already? (recorded AND on disk) — silent no-op.
        if db.pulled_bundle()?.as_deref() == Some(bundle.digest.as_str())
            && self.link_target_hex().as_deref() == Some(hex.as_str())
        {
            return Ok(self.link_path.clone());
        }

        // Presence in bundles/ == complete (module invariant): only
        // fetch + unpack when the content-addressed dir is missing.
        if !final_dir.is_dir() {
            let targz = source.fetch(bundle).await?;
            let tmp = self.work_dir.join(format!("unpack-{hex}"));
            if tmp.exists() {
                // Leftover from a failed attempt this process life
                // (startup wiped work/); rebuild from scratch.
                fs::remove_dir_all(&tmp)?;
            }
            unpack_targz(&targz, &tmp)?;
            validate_layout(&tmp)?;
            fsync_tree(&tmp)?;
            match fs::rename(&tmp, &final_dir) {
                Ok(()) => {}
                Err(_) if final_dir.is_dir() => {
                    // Lost a benign race with ourselves (resume path)
                    // — the complete dir is what matters.
                    let _ = fs::remove_dir_all(&tmp);
                }
                Err(e) => return Err(e.into()),
            }
            fsync_dir(&self.bundles_dir)?;
        }

        // THE atomic swap: pre-made relative symlink, one rename(2).
        // kill -9 before => old bundle; after => new bundle. Never
        // neither (Law 3).
        let tmp_link = self.work_dir.join(format!("link-{hex}"));
        let _ = fs::remove_file(&tmp_link);
        std::os::unix::fs::symlink(Path::new(BUNDLES_DIR).join(&hex), &tmp_link)?;
        fs::rename(&tmp_link, &self.link_path)?;
        fsync_dir(&self.data_dir)?;

        // Only now — after the swap — does the DB learn about it
        // (task contract; recover() rolls forward if we die here).
        db.record_bundle(
            &bundle.digest,
            "bundle-swapped",
            &format!("render bundle {} swapped into place", bundle.digest),
        )?;
        info!(digest = %bundle.digest, "render bundle swapped into place");

        self.gc(Some(&hex));
        Ok(self.link_path.clone())
    }
}

/// Unpack a verified render-bundle tar.gz
/// ([`RENDER_BUNDLE_MEDIA_TYPE`]) into `dest`, fail-closed:
/// - only regular files and directories (a config bundle has no
///   business carrying symlinks/devices — refuse, don't skip);
/// - every entry path must be strictly inside `dest` (no absolute
///   paths, no `..` — spec/reeve/08-packaging.md §10.7 posture);
/// - file bytes are fsynced as written (Law 3: the later rename
///   publishes only durable content).
fn unpack_targz(targz: &[u8], dest: &Path) -> Result<(), PullError> {
    fs::create_dir_all(dest)?;
    let mut archive = tar::Archive::new(GzDecoder::new(targz));
    let entries = archive
        .entries()
        .map_err(|e| PullError::Protocol(format!("unreadable tar.gz: {e}")))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| PullError::Protocol(format!("corrupt tar entry: {e}")))?;
        let raw = entry
            .path()
            .map_err(|e| PullError::Protocol(format!("bad tar path: {e}")))?
            .into_owned();
        let mut clean = PathBuf::new();
        for comp in raw.components() {
            match comp {
                Component::Normal(c) => clean.push(c),
                Component::CurDir => {}
                _ => {
                    return Err(PullError::BadLayout(format!(
                        "tar entry {} escapes the bundle dir",
                        raw.display()
                    )));
                }
            }
        }
        if clean.as_os_str().is_empty() {
            continue; // the "./" root entry
        }
        let target = dest.join(&clean);
        match entry.header().entry_type() {
            tar::EntryType::Directory => fs::create_dir_all(&target)?,
            tar::EntryType::Regular => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut file = File::create(&target)?;
                std::io::copy(&mut entry, &mut file)?;
                file.sync_all()?;
            }
            other => {
                return Err(PullError::BadLayout(format!(
                    "unsupported tar entry type {other:?} at {}",
                    clean.display()
                )));
            }
        }
    }
    Ok(())
}

/// Validate the D2 bundle layout contract
/// (docs/decisions/tree-render.md D2): `manifest.yaml` at the root;
/// if `apps/` exists, every entry under it is a directory (one app
/// dir = one unit of convergence).
fn validate_layout(dir: &Path) -> Result<(), PullError> {
    if !dir.join("manifest.yaml").is_file() {
        return Err(PullError::BadLayout(
            "bundle is missing manifest.yaml at its root (D2)".into(),
        ));
    }
    let apps = dir.join("apps");
    if apps.exists() {
        for entry in fs::read_dir(&apps)? {
            let entry = entry?;
            if !entry.path().is_dir() {
                return Err(PullError::BadLayout(format!(
                    "apps/{} is not a directory (D2: one app dir = one unit)",
                    entry.file_name().to_string_lossy()
                )));
            }
        }
    }
    Ok(())
}

/// fsync every directory under (and including) `root`; file contents
/// were synced at write time in [`unpack_targz`].
fn fsync_tree(root: &Path) -> Result<(), PullError> {
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            fsync_tree(&path)?;
        }
    }
    fsync_dir(root)
}

fn fsync_dir(dir: &Path) -> Result<(), PullError> {
    File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use reeve_types::reeve::manifest::RENDER_BUNDLE_MEDIA_TYPE;

    /// Build a tar.gz from (path, content) pairs; `None` content is
    /// a directory entry.
    fn targz(entries: &[(&str, Option<&[u8]>)]) -> Vec<u8> {
        let gz = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = tar::Builder::new(gz);
        for (path, content) in entries {
            let mut header = tar::Header::new_gnu();
            match content {
                Some(bytes) => {
                    header.set_size(bytes.len() as u64);
                    header.set_mode(0o644);
                    header.set_entry_type(tar::EntryType::Regular);
                    header.set_cksum();
                    builder.append_data(&mut header, path, *bytes).unwrap();
                }
                None => {
                    header.set_size(0);
                    header.set_mode(0o755);
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_cksum();
                    builder.append_data(&mut header, path, &[][..]).unwrap();
                }
            }
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    /// A valid D2-layout bundle tar.gz with one app.
    fn valid_bundle_targz() -> Vec<u8> {
        targz(&[
            ("manifest.yaml", Some(b"deviceId: dev-1\ngeneration: 1\n")),
            ("apps/", None),
            ("apps/web/", None),
            ("apps/web/compose.yml", Some(b"services: {}\n")),
            ("apps/web/files/", None),
            ("apps/web/files/app.conf", Some(b"key = value\n")),
        ])
    }

    /// Write a genuine OCI layout dir holding `layer` as the
    /// render-bundle layer of one artifact; returns the BundleRef.
    fn write_oci_layout(root: &Path, layer: &[u8], url: &str) -> BundleRef {
        let blobs = root.join("blobs").join("sha256");
        fs::create_dir_all(&blobs).unwrap();
        let put = |bytes: &[u8]| -> String {
            let digest = digest_bytes(bytes);
            fs::write(blobs.join(&digest["sha256:".len()..]), bytes).unwrap();
            digest
        };
        let layer_digest = put(layer);
        let config_bytes = b"{}".to_vec();
        let config_digest = put(&config_bytes);
        let manifest_json = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": OCI_MANIFEST_MEDIA_TYPE,
            "config": {
                "mediaType": "application/vnd.oci.empty.v1+json",
                "digest": config_digest,
                "size": config_bytes.len(),
            },
            "layers": [{
                "mediaType": RENDER_BUNDLE_MEDIA_TYPE,
                "digest": layer_digest,
                "size": layer.len(),
            }],
        })
        .to_string()
        .into_bytes();
        let manifest_digest = put(&manifest_json);
        fs::write(root.join("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();
        fs::write(
            root.join("index.json"),
            serde_json::json!({
                "schemaVersion": 2,
                "manifests": [{
                    "mediaType": OCI_MANIFEST_MEDIA_TYPE,
                    "digest": manifest_digest,
                    "size": manifest_json.len(),
                }],
            })
            .to_string(),
        )
        .unwrap();
        BundleRef {
            media_type: Some(RENDER_BUNDLE_MEDIA_TYPE.to_string()),
            digest: manifest_digest,
            size_bytes: Some(manifest_json.len() as u64),
            url: url.to_string(),
        }
    }

    struct Harness {
        _source_dir: tempfile::TempDir,
        _data_dir: tempfile::TempDir,
        source: BundleSource,
        store: BundleStore,
        db: AgentDb,
        bundle: BundleRef,
        layout_root: PathBuf,
    }

    /// dir:// harness: manifest-source dir with an `oci/` layout
    /// beside where manifest.json would live (CLAUDE.md Milestone 1
    /// shape), fresh data dir + agent.db.
    fn harness() -> Harness {
        let source_dir = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let layout_root = source_dir.path().join("oci");
        let bundle = write_oci_layout(&layout_root, &valid_bundle_targz(), "oci");
        let source =
            BundleSource::parse(&format!("dir://{}", source_dir.path().display()), None).unwrap();
        let store = BundleStore::open(data_dir.path()).unwrap();
        let db = AgentDb::open(&data_dir.path().join("agent.db")).unwrap();
        Harness {
            source,
            store,
            db,
            bundle,
            layout_root,
            _source_dir: source_dir,
            _data_dir: data_dir,
        }
    }

    fn journal_events(db: &AgentDb) -> Vec<String> {
        db.journal_entries()
            .unwrap()
            .into_iter()
            .map(|e| e.event)
            .collect()
    }

    #[tokio::test]
    async fn dir_pull_unpack_swap() {
        let mut h = harness();
        let path = h
            .store
            .apply(&mut h.db, &h.source, &h.bundle)
            .await
            .unwrap();
        // Swapped content readable through the symlink.
        assert_eq!(path, h.store.current_path());
        let manifest = fs::read_to_string(path.join("manifest.yaml")).unwrap();
        assert!(manifest.contains("dev-1"));
        assert_eq!(
            fs::read_to_string(path.join("apps/web/compose.yml")).unwrap(),
            "services: {}\n"
        );
        // Digest recorded only in agent.db + derivable from disk.
        assert_eq!(
            h.db.pulled_bundle().unwrap().as_deref(),
            Some(h.bundle.digest.as_str())
        );
        assert_eq!(
            h.store.current_digest().as_deref(),
            Some(h.bundle.digest.as_str())
        );
        assert!(journal_events(&h.db).contains(&"bundle-swapped".to_string()));
    }

    #[tokio::test]
    async fn corrupt_layer_fails_closed() {
        let mut h = harness();
        // Tamper the layer blob: same name, different bytes.
        let manifest_bytes = fs::read(
            h.layout_root
                .join("blobs/sha256")
                .join(&h.bundle.digest["sha256:".len()..]),
        )
        .unwrap();
        let m: OciManifest = serde_json::from_slice(&manifest_bytes).unwrap();
        let layer_hex = m.layers[0].digest["sha256:".len()..].to_string();
        fs::write(
            h.layout_root.join("blobs/sha256").join(&layer_hex),
            b"evil bytes",
        )
        .unwrap();

        let err = h
            .store
            .apply(&mut h.db, &h.source, &h.bundle)
            .await
            .unwrap_err();
        assert!(matches!(err, PullError::DigestMismatch { .. }), "{err}");
        // Nothing swapped, nothing recorded, failure journaled.
        assert_eq!(h.store.current_digest(), None);
        assert_eq!(h.db.pulled_bundle().unwrap(), None);
        assert!(journal_events(&h.db).contains(&"bundle-bad-digest".to_string()));
    }

    #[tokio::test]
    async fn corrupt_oci_manifest_fails_closed() {
        let mut h = harness();
        fs::write(
            h.layout_root
                .join("blobs/sha256")
                .join(&h.bundle.digest["sha256:".len()..]),
            br#"{"schemaVersion":2,"layers":[]}"#,
        )
        .unwrap();
        let err = h
            .store
            .apply(&mut h.db, &h.source, &h.bundle)
            .await
            .unwrap_err();
        assert!(matches!(err, PullError::DigestMismatch { .. }), "{err}");
        assert_eq!(h.store.current_digest(), None);
    }

    #[tokio::test]
    async fn partial_unpack_then_restart_resumes() {
        let mut h = harness();
        // Simulate kill -9 mid-unpack: junk in work/ (a partial
        // unpack dir and a stray temp file), nothing in bundles/.
        let junk_dir = h.store.work_dir.join("unpack-deadbeef");
        fs::create_dir_all(junk_dir.join("apps")).unwrap();
        fs::write(junk_dir.join("manifest.yaml"), b"partial").unwrap();
        fs::write(h.store.work_dir.join("blob.tmp"), b"partial download").unwrap();

        // Startup IS recovery: reopen store, recover, then sync.
        let store = BundleStore::open(&h.store.data_dir).unwrap();
        let recovered = store.recover(&mut h.db).unwrap();
        assert_eq!(recovered, None);
        assert_eq!(fs::read_dir(&store.work_dir).unwrap().count(), 0);

        // The interrupted pull resumes cleanly.
        store.apply(&mut h.db, &h.source, &h.bundle).await.unwrap();
        assert_eq!(
            store.current_digest().as_deref(),
            Some(h.bundle.digest.as_str())
        );
    }

    #[tokio::test]
    async fn crash_between_swap_and_record_rolls_forward() {
        let mut h = harness();
        h.store
            .apply(&mut h.db, &h.source, &h.bundle)
            .await
            .unwrap();
        // Simulate kill -9 after the symlink rename but before the
        // DB record: disk has the new bundle, the DB never heard.
        let db_path = h.store.data_dir.join("fresh-agent.db");
        let mut fresh_db = AgentDb::open(&db_path).unwrap();
        assert_eq!(fresh_db.pulled_bundle().unwrap(), None);

        let recovered = h.store.recover(&mut fresh_db).unwrap();
        assert_eq!(recovered.as_deref(), Some(h.bundle.digest.as_str()));
        assert_eq!(
            fresh_db.pulled_bundle().unwrap().as_deref(),
            Some(h.bundle.digest.as_str())
        );
        assert!(journal_events(&fresh_db).contains(&"bundle-rolled-forward".to_string()));
    }

    #[tokio::test]
    async fn recover_clears_record_when_disk_lost() {
        let mut h = harness();
        h.store
            .apply(&mut h.db, &h.source, &h.bundle)
            .await
            .unwrap();
        // External interference: bundle dir + link removed.
        fs::remove_file(h.store.current_path()).unwrap();
        fs::remove_dir_all(&h.store.bundles_dir).unwrap();
        fs::create_dir_all(&h.store.bundles_dir).unwrap();

        let recovered = h.store.recover(&mut h.db).unwrap();
        assert_eq!(recovered, None);
        assert_eq!(h.db.pulled_bundle().unwrap(), None);
        assert!(journal_events(&h.db).contains(&"bundle-state-cleared".to_string()));
    }

    #[tokio::test]
    async fn reapply_same_digest_is_a_silent_no_op() {
        let mut h = harness();
        h.store
            .apply(&mut h.db, &h.source, &h.bundle)
            .await
            .unwrap();
        // Remove the source entirely: a re-apply must not re-fetch.
        fs::remove_dir_all(&h.layout_root).unwrap();
        let path = h
            .store
            .apply(&mut h.db, &h.source, &h.bundle)
            .await
            .unwrap();
        assert!(path.join("manifest.yaml").is_file());
        // Exactly one swap journaled.
        let swaps = journal_events(&h.db)
            .iter()
            .filter(|e| *e == "bundle-swapped")
            .count();
        assert_eq!(swaps, 1);
    }

    #[tokio::test]
    async fn crash_after_rename_before_swap_resumes_without_refetch() {
        let mut h = harness();
        // Simulate: bundles/<hex> complete on disk (rename landed),
        // but no symlink swap and no record — then the source goes
        // away. Resume must complete from the complete dir alone.
        let hex = h.bundle.digest["sha256:".len()..].to_string();
        let tmp = h.store.work_dir.join(format!("unpack-{hex}"));
        unpack_targz(&valid_bundle_targz(), &tmp).unwrap();
        fs::rename(&tmp, h.store.bundles_dir.join(&hex)).unwrap();
        fs::remove_dir_all(&h.layout_root).unwrap(); // source gone

        h.store
            .apply(&mut h.db, &h.source, &h.bundle)
            .await
            .unwrap();
        assert_eq!(
            h.store.current_digest().as_deref(),
            Some(h.bundle.digest.as_str())
        );
        assert_eq!(
            h.db.pulled_bundle().unwrap().as_deref(),
            Some(h.bundle.digest.as_str())
        );
    }

    #[tokio::test]
    async fn new_bundle_swaps_and_gcs_old() {
        let mut h = harness();
        h.store
            .apply(&mut h.db, &h.source, &h.bundle)
            .await
            .unwrap();
        let old_hex = h.bundle.digest["sha256:".len()..].to_string();

        // Author a new bundle into a second layout.
        let new_targz = targz(&[
            ("manifest.yaml", Some(b"deviceId: dev-1\ngeneration: 2\n")),
            ("apps/", None),
            ("apps/db/", None),
            ("apps/db/compose.yml", Some(b"services: {}\n")),
        ]);
        fs::remove_dir_all(&h.layout_root).unwrap();
        let new_bundle = write_oci_layout(&h.layout_root, &new_targz, "oci");
        assert_ne!(new_bundle.digest, h.bundle.digest);

        let path = h
            .store
            .apply(&mut h.db, &h.source, &new_bundle)
            .await
            .unwrap();
        assert!(path.join("apps/db/compose.yml").is_file());
        assert!(!path.join("apps/web").exists());
        assert_eq!(
            h.db.pulled_bundle().unwrap().as_deref(),
            Some(new_bundle.digest.as_str())
        );
        // Old content-addressed dir GC'd; only the new one remains.
        assert!(!h.store.bundles_dir.join(&old_hex).exists());
        assert_eq!(fs::read_dir(&h.store.bundles_dir).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn traversal_tar_entry_fails_closed() {
        let mut h = harness();
        // tar::Builder itself refuses `..` paths, so forge the raw
        // GNU header a hostile bundle would carry.
        let gz = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        header.set_size(12);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        builder
            .append_data(&mut header, "manifest.yaml", &b"deviceId: x\n"[..])
            .unwrap();
        let mut evil_header = tar::Header::new_gnu();
        let name = b"../evil.txt";
        evil_header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name);
        evil_header.set_size(6);
        evil_header.set_mode(0o644);
        evil_header.set_entry_type(tar::EntryType::Regular);
        evil_header.set_cksum();
        builder.append(&evil_header, &b"escape"[..]).unwrap();
        let evil = builder.into_inner().unwrap().finish().unwrap();
        fs::remove_dir_all(&h.layout_root).unwrap();
        let bundle = write_oci_layout(&h.layout_root, &evil, "oci");
        let err = h
            .store
            .apply(&mut h.db, &h.source, &bundle)
            .await
            .unwrap_err();
        assert!(matches!(err, PullError::BadLayout(_)), "{err}");
        assert_eq!(h.store.current_digest(), None);
        assert!(journal_events(&h.db).contains(&"bundle-bad-layout".to_string()));
        // And nothing escaped above the unpack dir.
        assert!(!h.store.data_dir.join("evil.txt").exists());
        assert!(!h.store.work_dir.join("evil.txt").exists());
    }

    #[tokio::test]
    async fn symlink_tar_entry_fails_closed() {
        let mut h = harness();
        let gz = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        header.set_size(12);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        builder
            .append_data(&mut header, "manifest.yaml", &b"deviceId: x\n"[..])
            .unwrap();
        let mut link = tar::Header::new_gnu();
        link.set_size(0);
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_cksum();
        builder
            .append_link(&mut link, "apps/escape", "/etc/passwd")
            .unwrap();
        let evil = builder.into_inner().unwrap().finish().unwrap();

        fs::remove_dir_all(&h.layout_root).unwrap();
        let bundle = write_oci_layout(&h.layout_root, &evil, "oci");
        let err = h
            .store
            .apply(&mut h.db, &h.source, &bundle)
            .await
            .unwrap_err();
        assert!(matches!(err, PullError::BadLayout(_)), "{err}");
    }

    #[tokio::test]
    async fn missing_manifest_yaml_fails_closed() {
        let mut h = harness();
        let bad = targz(&[("apps/", None), ("apps/web/", None)]);
        fs::remove_dir_all(&h.layout_root).unwrap();
        let bundle = write_oci_layout(&h.layout_root, &bad, "oci");
        let err = h
            .store
            .apply(&mut h.db, &h.source, &bundle)
            .await
            .unwrap_err();
        assert!(matches!(err, PullError::BadLayout(_)), "{err}");
        assert_eq!(h.store.current_digest(), None);
    }

    #[tokio::test]
    async fn missing_layout_dir_is_unreachable() {
        let mut h = harness();
        fs::remove_dir_all(&h.layout_root).unwrap();
        let err = h
            .store
            .apply(&mut h.db, &h.source, &h.bundle)
            .await
            .unwrap_err();
        assert!(matches!(err, PullError::Unreachable(_)), "{err}");
        assert!(journal_events(&h.db).contains(&"bundle-pull-failed".to_string()));
    }

    #[tokio::test]
    async fn sync_pulls_the_accepted_manifests_bundle() {
        use reeve_types::reeve::manifest::{ManifestVersion, StateManifest};
        let mut h = harness();
        // No accepted manifest yet: nothing to do.
        assert!(h.store.sync(&mut h.db, &h.source).await.unwrap().is_none());

        h.db.record_accepted(
            &StateManifest {
                manifest_version: ManifestVersion(1),
                bundle: Some(h.bundle.clone()),
                apps: vec![],
            },
            "sha256:etag",
            Severity::Info,
            "manifest-accepted",
            "",
        )
        .unwrap();
        let path = h.store.sync(&mut h.db, &h.source).await.unwrap().unwrap();
        assert!(path.join("manifest.yaml").is_file());

        // bundle: null (zero apps) — nothing to pull; B3 owns removal.
        h.db.record_accepted(
            &StateManifest {
                manifest_version: ManifestVersion(2),
                bundle: None,
                apps: vec![],
            },
            "sha256:etag2",
            Severity::Info,
            "manifest-accepted",
            "",
        )
        .unwrap();
        assert!(h.store.sync(&mut h.db, &h.source).await.unwrap().is_none());
    }

    /// Standard OCI distribution pull over HTTP: GET
    /// /v2/<name>/manifests/<digest> and /v2/<name>/blobs/<digest>
    /// with the device bearer token (spec/reeve/08-packaging.md
    /// §10.2; docs/decisions/delivery.md D7).
    #[tokio::test]
    async fn http_v2_pull_with_device_token() {
        use axum::extract::{Path as AxPath, State};
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::get;

        let layout = tempfile::tempdir().unwrap();
        let bundle_ref = write_oci_layout(layout.path(), &valid_bundle_targz(), "/v2/render-dev-1");
        let blobs = layout.path().join("blobs").join("sha256");

        async fn serve_blob(
            State(blobs): State<PathBuf>,
            AxPath((_name, digest)): AxPath<(String, String)>,
            headers: HeaderMap,
        ) -> Result<Vec<u8>, StatusCode> {
            if headers.get("authorization").and_then(|v| v.to_str().ok())
                != Some("Bearer tok-dev-1")
            {
                return Err(StatusCode::UNAUTHORIZED);
            }
            let hex = digest.strip_prefix("sha256:").ok_or(StatusCode::NOT_FOUND)?;
            fs::read(blobs.join(hex)).map_err(|_| StatusCode::NOT_FOUND)
        }

        // Manifests are content-addressed blobs too — one handler.
        let app = axum::Router::new()
            .route("/v2/{name}/manifests/{reference}", get(serve_blob))
            .route("/v2/{name}/blobs/{digest}", get(serve_blob))
            .with_state(blobs);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let data_dir = tempfile::tempdir().unwrap();
        let store = BundleStore::open(data_dir.path()).unwrap();
        let mut db = AgentDb::open(&data_dir.path().join("agent.db")).unwrap();
        let source = BundleSource::parse(
            &format!("http://{addr}"),
            Some("tok-dev-1".to_string()),
        )
        .unwrap();

        let path = store.apply(&mut db, &source, &bundle_ref).await.unwrap();
        assert_eq!(
            fs::read_to_string(path.join("apps/web/compose.yml")).unwrap(),
            "services: {}\n"
        );
        assert_eq!(
            db.pulled_bundle().unwrap().as_deref(),
            Some(bundle_ref.digest.as_str())
        );

        // Wrong token: fail closed, current state untouched.
        let bad = BundleSource::parse(&format!("http://{addr}"), Some("wrong".into())).unwrap();
        let new_ref = BundleRef {
            digest: format!("sha256:{}", "9".repeat(64)),
            ..bundle_ref.clone()
        };
        let err = store.apply(&mut db, &bad, &new_ref).await.unwrap_err();
        assert!(matches!(err, PullError::Protocol(_)), "{err}");
        assert_eq!(
            store.current_digest().as_deref(),
            Some(bundle_ref.digest.as_str())
        );
    }

    #[test]
    fn http_repo_base_resolution() {
        let s = HttpBundleSource::new("https://reeve.example", None);
        assert_eq!(
            s.repo_base("/v2/render/dev-1"),
            "https://reeve.example/v2/render/dev-1"
        );
        assert_eq!(
            s.repo_base("https://other.example/v2/x"),
            "https://other.example/v2/x"
        );
        // Full manifests URL trims back to the repo base.
        let d = format!("sha256:{}", "a".repeat(64));
        assert_eq!(
            s.repo_base(&format!("/v2/render/dev-1/manifests/{d}")),
            "https://reeve.example/v2/render/dev-1"
        );
    }

    #[test]
    fn bundle_source_parse_schemes() {
        assert!(matches!(
            BundleSource::parse("dir:///opt/src", None).unwrap(),
            BundleSource::Dir(_)
        ));
        assert!(matches!(
            BundleSource::parse("https://reeve.example", Some("t".into())).unwrap(),
            BundleSource::Http(_)
        ));
        assert!(BundleSource::parse("oci://reg/x", None).is_err());
    }
}

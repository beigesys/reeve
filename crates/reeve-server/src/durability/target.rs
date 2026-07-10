//! The durability target: one `object_store` handle, four backends
//! (AWS S3, rustfs, MinIO via s3://; local filesystem path for the
//! test + air-gap tier) — spec/reeve/07-durability.md §9.2: "one crate,
//! four targets, zero bespoke transports".
//!
//! Key layout under `reeve/<instance>/`:
//! - `epoch` — restore-fencing epoch marker (§9.5; lives AT THE
//!   TARGET, not in the DB)
//! - `gen/<genid>.db` — AEAD-sealed snapshot; genid is
//!   `<utc-timestamp>-<schema>` (§9.2)
//! - `gen/<genid>/cs/<seq>` — AEAD-sealed, gzip'd changesets, strictly
//!   sequenced from 1 (§9.3)
//! - `gen/latest` — pointer JSON, written LAST (§9.2)
//!
//! Atomic-or-absent (§9.2, Law 3 extended to the bucket): S3 PUT is
//! atomic by contract; `object_store`'s LocalFileSystem stages every
//! put to a temp path and renames — a process killed at any byte
//! leaves no partial object at a final key. On top of that, nothing
//! references a new generation until `gen/latest` (written last)
//! points at it, so a crash between payload and pointer leaves the
//! previous generation authoritative.

use std::sync::Arc;

use anyhow::{Context as _, bail};
use chrono::Utc;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, PutPayload};
use serde::{Deserialize, Serialize};

/// The `gen/latest` pointer body (§9.2): the ONLY entry point restore
/// trusts, so partially-shipped generations are invisible.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatestPointer {
    pub generation: String,
    pub snapshot_key: String,
    pub schema: i64,
    /// Unix seconds at snapshot time (recency checks, §9.4).
    pub created_at: i64,
}

pub struct Target {
    store: Arc<dyn ObjectStore>,
}

impl Target {
    /// Open a target from its configured url (§9.2
    /// `durability.target.url`): `s3://bucket/prefix` (credentials and
    /// endpoint from the standard AWS_* / OBJECT_STORE env vars —
    /// covers MinIO/rustfs via AWS_ENDPOINT), `file:///abs/path`, or a
    /// plain filesystem path. All keys live under `reeve/<instance>/`.
    pub fn open(url: &str, instance: &str) -> anyhow::Result<Self> {
        let (store, base): (Box<dyn ObjectStore>, ObjPath) = match url::Url::parse(url) {
            Ok(parsed) if parsed.scheme() == "file" => {
                let dir = parsed
                    .to_file_path()
                    .map_err(|()| anyhow::anyhow!("bad file:// url {url:?}"))?;
                std::fs::create_dir_all(&dir)?;
                (
                    Box::new(object_store::local::LocalFileSystem::new_with_prefix(dir)?),
                    ObjPath::default(),
                )
            }
            Ok(parsed) => object_store::parse_url_opts(&parsed, std::env::vars())
                .with_context(|| format!("opening durability target {url:?}"))?,
            // Not a url: a plain local filesystem path (air-gap tier).
            Err(_) => {
                std::fs::create_dir_all(url)?;
                (
                    Box::new(object_store::local::LocalFileSystem::new_with_prefix(url)?),
                    ObjPath::default(),
                )
            }
        };
        let prefix = if base.parts().next().is_none() {
            ObjPath::from(format!("reeve/{instance}"))
        } else {
            ObjPath::from(format!("{base}/reeve/{instance}"))
        };
        Ok(Target {
            store: Arc::new(object_store::prefix::PrefixStore::new(store, prefix)),
        })
    }

    // ---- key layout ----

    pub fn epoch_key() -> ObjPath {
        ObjPath::from("epoch")
    }
    pub fn latest_key() -> ObjPath {
        ObjPath::from("gen/latest")
    }
    pub fn snapshot_key(generation: &str) -> ObjPath {
        ObjPath::from(format!("gen/{generation}.db"))
    }
    pub fn changeset_key(generation: &str, seq: u64) -> ObjPath {
        ObjPath::from(format!("gen/{generation}/cs/{seq:08}"))
    }

    /// Mint a generation id: UTC timestamp + schema version (§9.2
    /// `<rfc3339>-<schema>`; basic ISO-8601 format — `:`-free so the id
    /// is a safe object key and filename on every backend).
    pub fn new_generation_id(schema: i64) -> String {
        format!("{}-{schema}", Utc::now().format("%Y%m%dT%H%M%S%3fZ"))
    }

    // ---- object ops ----

    pub async fn put(&self, key: &ObjPath, bytes: Vec<u8>) -> anyhow::Result<()> {
        self.store
            .put(key, PutPayload::from(bytes))
            .await
            .with_context(|| format!("uploading {key}"))?;
        Ok(())
    }

    pub async fn get(&self, key: &ObjPath) -> anyhow::Result<Vec<u8>> {
        Ok(self
            .store
            .get(key)
            .await
            .with_context(|| format!("fetching {key}"))?
            .bytes()
            .await?
            .to_vec())
    }

    pub async fn get_opt(&self, key: &ObjPath) -> anyhow::Result<Option<Vec<u8>>> {
        match self.store.get(key).await {
            Ok(res) => Ok(Some(res.bytes().await?.to_vec())),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e).with_context(|| format!("fetching {key}")),
        }
    }

    pub async fn delete(&self, key: &ObjPath) -> anyhow::Result<()> {
        match self.store.delete(key).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e).with_context(|| format!("deleting {key}")),
        }
    }

    /// Read the latest-generation pointer, if any generation was ever
    /// fully shipped.
    pub async fn latest(&self) -> anyhow::Result<Option<LatestPointer>> {
        match self.get_opt(&Self::latest_key()).await? {
            Some(bytes) => Ok(Some(
                serde_json::from_slice(&bytes).context("gen/latest pointer is corrupt")?,
            )),
            None => Ok(None),
        }
    }

    /// All snapshot generation ids present at the target (finalized
    /// `gen/<genid>.db` objects only — staged/partial uploads never
    /// match), with upload timestamps (unix seconds).
    pub async fn list_generations(&self) -> anyhow::Result<Vec<(String, i64)>> {
        let listing = self
            .store
            .list_with_delimiter(Some(&ObjPath::from("gen")))
            .await?;
        let mut out = Vec::new();
        for obj in listing.objects {
            if let Some(name) = obj.location.filename()
                && let Some(genid) = name.strip_suffix(".db")
            {
                out.push((genid.to_string(), obj.last_modified.timestamp()));
            }
        }
        out.sort();
        Ok(out)
    }

    /// The changesets chained to a generation, sorted by sequence,
    /// asserted CONTIGUOUS from 1 — a gap means the chain is unusable
    /// past it (§9.3 strict sequencing). Returns (seq, key,
    /// uploaded_at_unix).
    pub async fn list_changesets(
        &self,
        generation: &str,
    ) -> anyhow::Result<Vec<(u64, ObjPath, i64)>> {
        let dir = ObjPath::from(format!("gen/{generation}/cs"));
        let listing = self.store.list_with_delimiter(Some(&dir)).await?;
        let mut out: Vec<(u64, ObjPath, i64)> = Vec::new();
        for obj in listing.objects {
            let Some(name) = obj.location.filename() else {
                continue;
            };
            let seq: u64 = name
                .parse()
                .with_context(|| format!("non-sequence object {name:?} under {dir}"))?;
            out.push((seq, obj.location.clone(), obj.last_modified.timestamp()));
        }
        out.sort_by_key(|(seq, _, _)| *seq);
        for (i, (seq, _, _)) in out.iter().enumerate() {
            if *seq != (i as u64) + 1 {
                bail!(
                    "changeset sequence for generation {generation} has a gap: \
                     expected seq {}, found {seq}",
                    i + 1
                );
            }
        }
        Ok(out)
    }

    /// Delete one whole generation: snapshot + every chained changeset
    /// (§9.2: pruning removes whole generations).
    pub async fn delete_generation(&self, generation: &str) -> anyhow::Result<()> {
        let dir = ObjPath::from(format!("gen/{generation}/cs"));
        if let Ok(listing) = self.store.list_with_delimiter(Some(&dir)).await {
            for obj in listing.objects {
                self.delete(&obj.location).await?;
            }
        }
        self.delete(&Self::snapshot_key(generation)).await
    }

    // ---- epoch fencing (§9.5) ----

    /// Read the epoch marker at the target; absent means 0.
    pub async fn read_epoch(&self) -> anyhow::Result<Option<u16>> {
        match self.get_opt(&Self::epoch_key()).await? {
            Some(bytes) => {
                let s = String::from_utf8(bytes).context("epoch marker is not UTF-8")?;
                Ok(Some(
                    s.trim().parse().context("epoch marker is not an integer")?,
                ))
            }
            None => Ok(None),
        }
    }

    /// Write the epoch marker. Restore ordering (§9.5): callers MUST
    /// increment this AT THE TARGET before serving under the new epoch.
    pub async fn write_epoch(&self, epoch: u16) -> anyhow::Result<()> {
        self.put(&Self::epoch_key(), epoch.to_string().into_bytes())
            .await
    }
}

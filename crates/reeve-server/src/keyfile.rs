//! The D15 external master keyfile: `REEVE_DATA/secret.key`, 32 raw
//! bytes, mode 0600 — the ONE key custody story for everything shipped
//! off-box (spec/reeve/07-durability.md §9.1/§9.6: snapshots and
//! changesets are AEAD-encrypted under it; spec/reeve/10-secrets.md
//! §12.2: the secrets vault reuses the same file — C7).
//!
//! Crash-only (Law 3, D6 file-write rule): creation is temp + fsync +
//! rename, so a kill -9 mid-create leaves either no keyfile or a
//! complete one — never a short read.

use std::io::Write as _;
use std::path::Path;

use anyhow::{Context as _, bail};

/// Key length in bytes (XChaCha20-Poly1305 / ChaCha20-Poly1305 key size).
pub const KEY_LEN: usize = 32;

/// Filename under the data dir (spec/reeve/07-durability.md §9.1).
pub const KEY_FILE_NAME: &str = "secret.key";

/// Load the keyfile, creating it (with fresh random bytes, mode 0600)
/// if absent. Idempotent — safe on every startup.
pub fn load_or_create(path: &Path) -> anyhow::Result<[u8; KEY_LEN]> {
    match load(path) {
        Ok(key) => Ok(key),
        Err(e) if !path.exists() => {
            create(path).with_context(|| format!("creating keyfile {} ({e})", path.display()))
        }
        Err(e) => Err(e),
    }
}

/// Load an EXISTING keyfile — the restore path (§9.5: DR needs two
/// artifacts, snapshot + keyfile; a missing keyfile must be a loud,
/// actionable error, never silently re-minted, which would make every
/// shipped ciphertext unreadable).
pub fn load(path: &Path) -> anyhow::Result<[u8; KEY_LEN]> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading keyfile {}", path.display()))?;
    if bytes.len() != KEY_LEN {
        bail!(
            "keyfile {} is {} bytes, expected {KEY_LEN} — corrupt or not a reeve keyfile",
            path.display(),
            bytes.len()
        );
    }
    let mut key = [0u8; KEY_LEN];
    key.copy_from_slice(&bytes);
    Ok(key)
}

fn create(path: &Path) -> anyhow::Result<[u8; KEY_LEN]> {
    let mut key = [0u8; KEY_LEN];
    getrandom::fill(&mut key).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;

    let dir = path.parent().context("keyfile path has no parent dir")?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{KEY_FILE_NAME}.tmp-{}", std::process::id()));

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(&tmp)
        .with_context(|| format!("creating temp keyfile {}", tmp.display()))?;
    f.write_all(&key)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming keyfile into place at {}", path.display()))?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_then_load_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(KEY_FILE_NAME);
        let a = load_or_create(&path).unwrap();
        let b = load_or_create(&path).unwrap();
        assert_eq!(a, b, "second call loads, never re-mints");
        let c = load(&path).unwrap();
        assert_eq!(a, c);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn load_missing_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(&dir.path().join("nope.key")).is_err());
    }

    #[test]
    fn wrong_length_is_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(KEY_FILE_NAME);
        std::fs::write(&path, b"short").unwrap();
        assert!(load(&path).unwrap_err().to_string().contains("bytes"));
    }
}

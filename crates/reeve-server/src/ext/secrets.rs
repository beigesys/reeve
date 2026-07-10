//! ext-secrets (REV-009, C7) — the server-side secrets vault.
//!
//! Normative sources:
//! - spec/reeve/10-secrets.md §12.2 storage: secrets live in a table in
//!   reeve-server's SQLite, AEAD-encrypted under the master key in a
//!   FILE OUTSIDE the DB (`REEVE_DATA/secret.key`, D15 keyfile — the
//!   same file the durability tier uses). Snapshots ship ciphertext
//!   only. The same store holds the server's own operational secrets
//!   under the reserved `reeve-internal` scope.
//! - §12.1 scoping: secrets are defined at layers and resolve down the
//!   same chain as config (fleet -> class -> region -> site -> device,
//!   deeper wins). Resolution is SERVER-SIDE AT REQUEST TIME — render
//!   stays pure and bundles stay secret-free.
//! - §12.3 delivery: `POST /api/reeve/v1/secrets/resolve` over the
//!   device's enrollment credential; a device can only ask as itself
//!   and receives only its own resolution. Plaintext exists only in
//!   server RAM during resolve (and TLS in flight / the agent's env
//!   files) — this module never journals, logs, or persists a value.
//! - §12.4 rotation: rotating a secret bumps its version => affected
//!   devices' per-app `secrets_version` (hash of resolved
//!   name+version pairs, never values) changes => manifestVersion
//!   bumps. render.rs calls [`app_secrets_versions`] for that hash;
//!   the write routes here kick a render pass to propagate.
//! - §12.6 security: the resolve endpoint is the single plaintext
//!   egress, scoped to the requesting device, audit-countable
//!   (metadata only). The operator API is write-only: set, rotate,
//!   delete, list metadata — never read back, not even to admin.
//!
//! Crash-only (Law 3): every vault write is a single SQLite statement
//! (upsert/delete); the keyfile is created via temp+fsync+rename
//! (keyfile.rs). A kill -9 between a secret write and the render kick
//! is healed by the next render pass — [`app_secrets_versions`] always
//! recomputes from current table state, and per-device change
//! detection makes the catch-up bump exactly once.

use std::collections::{BTreeMap, BTreeSet};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse as _, Response};
use desired_state::FileSet;
use device_api::{DeviceIdentity, Identity, Role};
use reeve_types::margo::deployment::ApplicationDeployment;
use reeve_types::reeve::secrets::{
    ResolvedSecret, SecretsResolveRequest, SecretsResolveResponse,
};
use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{info, warn};

use crate::db::now_secs;
use crate::durability::aead;
use crate::keyfile::{self, KEY_LEN};
use crate::state::AppState;

/// The reserved scope for the server's OWN operational secrets (zot
/// upstream credentials, S3 keys, tier tokens — §12.2). Never on any
/// device's resolution chain: devices can never resolve it.
pub const INTERNAL_SCOPE: &str = "reeve-internal";

/// The in-band secret reference convention carried by rendered
/// parameter values (§12.1, D15 wire-exactness): `${secret:<name>}`.
/// MUST match reeve-agent's ext/secrets.rs parser — same open token,
/// same `}` terminator, no nesting.
const REF_OPEN: &str = "${secret:";

// ---------------------------------------------------------------- vault

/// Load (or create at first use) the vault master key — the D15
/// keyfile shared with the durability tier (§12.2: one key custody
/// story; restore = snapshot + keyfile).
pub fn vault_key(data_dir: &std::path::Path) -> anyhow::Result<[u8; KEY_LEN]> {
    keyfile::load_or_create(&data_dir.join(keyfile::KEY_FILE_NAME))
}

/// Validate a secret scope: `fleet`, `class.<n>`, `region.<n>`,
/// `site.<n>`, `device.<id>`, or the reserved [`INTERNAL_SCOPE`].
pub fn valid_scope(scope: &str) -> bool {
    if scope == "fleet" || scope == INTERNAL_SCOPE {
        return true;
    }
    ["class.", "region.", "site.", "device."]
        .iter()
        .any(|p| scope.strip_prefix(p).is_some_and(|rest| !rest.is_empty()))
}

/// Validate a secret name: what fits inside `${secret:<name>}` without
/// ambiguity for the agent's parser (no `}`, no whitespace/newlines).
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Set or rotate a secret: one atomic upsert (Law 3). A new (name,
/// scope) starts at version 1; an existing one gets `version + 1` and
/// `rotated_at` (§12.4 rotation = version bump). Returns the stored
/// version. The plaintext `value` exists only in RAM here.
pub fn put(
    conn: &Connection,
    key: &[u8; KEY_LEN],
    name: &str,
    scope: &str,
    value: &str,
) -> anyhow::Result<u64> {
    let ciphertext = aead::seal(key, value.as_bytes())?;
    conn.execute(
        "INSERT INTO secrets (name, scope, version, ciphertext, created_at)
         VALUES (?1, ?2, 1, ?3, ?4)
         ON CONFLICT(name, scope) DO UPDATE SET
             version = version + 1,
             ciphertext = excluded.ciphertext,
             rotated_at = excluded.created_at",
        params![name, scope, ciphertext, now_secs()],
    )?;
    let version: i64 = conn.query_row(
        "SELECT version FROM secrets WHERE name = ?1 AND scope = ?2",
        params![name, scope],
        |r| r.get(0),
    )?;
    Ok(version as u64)
}

/// Delete a secret. Returns whether a row existed. Idempotent.
pub fn delete(conn: &Connection, name: &str, scope: &str) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "DELETE FROM secrets WHERE name = ?1 AND scope = ?2",
        params![name, scope],
    )?;
    Ok(n > 0)
}

/// One secret's METADATA — the only readable surface after entry
/// (§12.2 write-only: name, scope, version, timestamps; never values).
#[derive(Debug, Clone, Serialize)]
pub struct SecretInfo {
    pub name: String,
    pub scope: String,
    pub version: u64,
    pub created_at: i64,
    pub rotated_at: Option<i64>,
}

/// List all secrets' metadata, ordered by (name, scope).
pub fn list(conn: &Connection) -> rusqlite::Result<Vec<SecretInfo>> {
    let mut stmt = conn.prepare(
        "SELECT name, scope, version, created_at, rotated_at
         FROM secrets ORDER BY name, scope",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(SecretInfo {
            name: r.get(0)?,
            scope: r.get(1)?,
            version: r.get::<_, i64>(2)? as u64,
            created_at: r.get(3)?,
            rotated_at: r.get(4)?,
        })
    })?;
    rows.collect()
}

// ----------------------------------------------------------- resolution

/// A device's scope chain, DEEPEST FIRST (§12.1 "deeper wins"), from
/// its device row's layer-chain membership (tree-render.md D11 — the
/// same chain render.rs merges config down):
/// `device.<id>` -> `site.<s>`? -> `region.<r>`? -> `class.<c>`? ->
/// `fleet`. [`INTERNAL_SCOPE`] is never on the chain.
pub fn device_chain(
    device_id: &str,
    class: Option<&str>,
    region: Option<&str>,
    site: Option<&str>,
) -> Vec<String> {
    let mut chain = vec![format!("device.{device_id}")];
    if let Some(s) = site {
        chain.push(format!("site.{s}"));
    }
    if let Some(r) = region {
        chain.push(format!("region.{r}"));
    }
    if let Some(c) = class {
        chain.push(format!("class.{c}"));
    }
    chain.push("fleet".to_string());
    chain
}

/// Resolve `names` to their VERSIONS down `chain` (deepest-first;
/// first scope with a row wins). No key needed — versions are audit
/// metadata, not values (§12.6). Unknown names are simply absent.
pub fn resolve_versions(
    conn: &Connection,
    chain: &[String],
    names: &BTreeSet<String>,
) -> rusqlite::Result<BTreeMap<String, u64>> {
    let mut out = BTreeMap::new();
    for name in names {
        for scope in chain {
            let version: Option<i64> = conn
                .query_row(
                    "SELECT version FROM secrets WHERE name = ?1 AND scope = ?2",
                    params![name, scope],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(v) = version {
                out.insert(name.clone(), v as u64);
                break; // deeper wins — stop at the first (deepest) hit
            }
        }
    }
    Ok(out)
}

/// Resolve `names` to plaintext VALUES down `chain` (§12.3). The
/// returned map exists only in RAM; the caller serializes it straight
/// into the resolve response and drops it. Unknown/unscoped names are
/// OMITTED, never an error (§12.6: not a secret-existence oracle). A
/// ciphertext that fails AEAD authentication (wrong keyfile, corrupt
/// row) is a loud server-side error — never silently omitted.
pub fn resolve_values(
    conn: &Connection,
    key: &[u8; KEY_LEN],
    chain: &[String],
    names: &BTreeSet<String>,
) -> anyhow::Result<BTreeMap<String, ResolvedSecret>> {
    let mut out = BTreeMap::new();
    for name in names {
        for scope in chain {
            let row: Option<(Vec<u8>, i64)> = conn
                .query_row(
                    "SELECT ciphertext, version FROM secrets
                     WHERE name = ?1 AND scope = ?2",
                    params![name, scope],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((ciphertext, version)) = row {
                let plaintext = aead::open(key, &ciphertext).map_err(|e| {
                    anyhow::anyhow!("secret {name:?} scope {scope:?}: {e}")
                })?;
                out.insert(
                    name.clone(),
                    ResolvedSecret {
                        value: String::from_utf8(plaintext)
                            .map_err(|_| anyhow::anyhow!("secret {name:?}: not UTF-8"))?,
                        version: version as u64,
                    },
                );
                break;
            }
        }
    }
    Ok(out)
}

// ------------------------------------------ per-app secrets_version

/// Collect every `${secret:<name>}` reference in `text` — the SAME
/// parser as reeve-agent's ext/secrets.rs `collect_secret_refs` (the
/// two sides MUST agree on what constitutes a reference, §12.3/§12.4).
pub fn collect_secret_refs(text: &str, out: &mut BTreeSet<String>) {
    let mut rest = text;
    while let Some(start) = rest.find(REF_OPEN) {
        let after = &rest[start + REF_OPEN.len()..];
        let Some(end) = after.find('}') else { break };
        out.insert(after[..end].to_string());
        rest = &after[end + 1..];
    }
}

/// Secret references in one rendered `deployment.yaml`: every
/// `${secret:<name>}` inside parameter VALUES (the literal template
/// desired-state renders verbatim, §12.1). Strings nested in
/// sequence/mapping values are scanned too — a superset of what the
/// agent env-materializes, which is safe: an extra name only widens
/// the rotation-bump set, never narrows it.
fn deployment_secret_refs(yaml: &[u8]) -> BTreeSet<String> {
    let mut refs = BTreeSet::new();
    let Ok(text) = std::str::from_utf8(yaml) else {
        return refs;
    };
    let Ok(deployment) = serde_yaml_ng::from_str::<ApplicationDeployment>(text) else {
        // desired-state emitted it, so this is unexpected — but the
        // render must not fail over it; the agent surfaces the error.
        return refs;
    };
    for parameter in deployment.spec.parameters.values() {
        if let Some(value) = &parameter.value {
            collect_value_refs(value, &mut refs);
        }
    }
    refs
}

fn collect_value_refs(value: &serde_yaml_ng::Value, out: &mut BTreeSet<String>) {
    match value {
        serde_yaml_ng::Value::String(s) => collect_secret_refs(s, out),
        serde_yaml_ng::Value::Sequence(seq) => {
            seq.iter().for_each(|v| collect_value_refs(v, out));
        }
        serde_yaml_ng::Value::Mapping(map) => {
            map.iter().for_each(|(_, v)| collect_value_refs(v, out));
        }
        _ => {}
    }
}

/// Deterministic hash of a resolved (name, version) set — the value of
/// the manifest's per-app `secretsVersion` (§12.4: "hash of resolved
/// secret names+versions, never values"). Length-prefixed like
/// render.rs's content_digest so boundaries are unambiguous.
pub fn secrets_version_hash(resolved: &BTreeMap<String, u64>) -> String {
    use sha2::{Digest as _, Sha256};
    let mut h = Sha256::new();
    for (name, version) in resolved {
        h.update((name.len() as u64).to_le_bytes());
        h.update(name.as_bytes());
        h.update(version.to_le_bytes());
    }
    format!("sha256:{:x}", h.finalize())
}

/// The render.rs hook (§12.4): per-app `secrets_version` for one
/// device's rendered file set. An app appears iff its rendered
/// `deployment.yaml` carries at least one `${secret:<name>}`
/// reference; the hash covers the (name, version) pairs that RESOLVE
/// down the device's chain — so creating, rotating, or deleting a
/// referenced secret each change the hash (and unresolved references
/// hash stably until the secret exists).
pub fn app_secrets_versions(
    conn: &Connection,
    files: &FileSet,
    chain: &[String],
) -> rusqlite::Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for (path, bytes) in files {
        let Some(app) = path
            .strip_prefix("apps/")
            .and_then(|rest| rest.strip_suffix("/deployment.yaml"))
        else {
            continue;
        };
        if app.contains('/') {
            continue; // only apps/<name>/deployment.yaml, not deeper
        }
        let refs = deployment_secret_refs(bytes);
        if refs.is_empty() {
            continue;
        }
        let resolved = resolve_versions(conn, chain, &refs)?;
        out.insert(app.to_string(), secrets_version_hash(&resolved));
    }
    Ok(out)
}

// ------------------------------------- server operational secrets

/// Read one of the server's OWN operational secrets (reserved
/// [`INTERNAL_SCOPE`], §12.2). `None` when unset.
pub fn internal_secret(
    conn: &Connection,
    key: &[u8; KEY_LEN],
    name: &str,
) -> anyhow::Result<Option<String>> {
    let chain = [INTERNAL_SCOPE.to_string()];
    let names = BTreeSet::from([name.to_string()]);
    Ok(resolve_values(conn, key, &chain, &names)?
        .remove(name)
        .map(|s| s.value))
}

/// Typed getter: zot upstream registry credentials
/// (`zot.upstream.username` / `zot.upstream.password`).
pub fn zot_upstream_credentials(
    conn: &Connection,
    key: &[u8; KEY_LEN],
) -> anyhow::Result<Option<(String, String)>> {
    let user = internal_secret(conn, key, "zot.upstream.username")?;
    let pass = internal_secret(conn, key, "zot.upstream.password")?;
    Ok(user.zip(pass))
}

/// Typed getter: S3 credentials for the durability target
/// (`s3.access-key-id` / `s3.secret-access-key`).
pub fn s3_credentials(
    conn: &Connection,
    key: &[u8; KEY_LEN],
) -> anyhow::Result<Option<(String, String)>> {
    let id = internal_secret(conn, key, "s3.access-key-id")?;
    let secret = internal_secret(conn, key, "s3.secret-access-key")?;
    Ok(id.zip(secret))
}

/// Typed getter: the token this tier presents to its upstream tier
/// (`tier.<name>.token`, federation §8 — consumed by C10).
pub fn tier_token(
    conn: &Connection,
    key: &[u8; KEY_LEN],
    tier: &str,
) -> anyhow::Result<Option<String>> {
    internal_secret(conn, key, &format!("tier.{tier}.token"))
}

// --------------------------------------------------------------- routes

/// Flag the render pipeline dirty (render.rs [`crate::render::RENDER_DIRTY_KEY`]):
/// set in the SAME transaction as a vault write so a kill -9 before the
/// propagating render pass is healed by startup reconcile (Law 3).
fn mark_render_dirty(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, '1')
         ON CONFLICT(key) DO UPDATE SET value = '1'",
        params![crate::render::RENDER_DIRTY_KEY],
    )?;
    Ok(())
}

fn internal_error(e: impl std::fmt::Display) -> Response {
    // Error text here never contains a value: vault errors carry
    // name/scope metadata only.
    warn!(error = %e, "secrets route internal error");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

/// POST /api/reeve/v1/secrets/resolve (device auth; §12.3/§12.6).
/// The device receives exactly its own resolution: names resolve down
/// ITS chain; unknown/unscoped names are absent from the response.
pub async fn resolve_route(
    State(state): State<AppState>,
    DeviceIdentity(device_id): DeviceIdentity,
    Json(req): Json<SecretsResolveRequest>,
) -> Response {
    let key = match vault_key(&state.cfg.data_dir) {
        Ok(k) => k,
        Err(e) => return internal_error(e),
    };
    let names: BTreeSet<String> = req.secrets.into_iter().collect();

    let secrets = {
        let conn = state.db.lock().expect("db mutex poisoned");
        let row: Option<(Option<String>, Option<String>, Option<String>)> = match conn
            .query_row(
                "SELECT class, region, site FROM devices WHERE device_id = ?1",
                params![device_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
        {
            Ok(r) => r,
            Err(e) => return internal_error(e),
        };
        let Some((class, region, site)) = row else {
            // Authenticated token for a vanished device row.
            return StatusCode::NOT_FOUND.into_response();
        };
        let chain = device_chain(
            &device_id,
            class.as_deref(),
            region.as_deref(),
            site.as_deref(),
        );
        match resolve_values(&conn, &key, &chain, &names) {
            Ok(s) => s,
            Err(e) => return internal_error(e),
        }
    };

    // Audit-countable (§12.6): who resolved what version when —
    // metadata, never values.
    let audit: Vec<String> = secrets
        .iter()
        .map(|(name, s)| format!("{name}@{}", s.version))
        .collect();
    info!(
        device = %device_id,
        requested = names.len(),
        resolved = %audit.join(" "),
        "secrets resolved"
    );

    Json(SecretsResolveResponse { secrets }).into_response()
}

/// Body of PUT /api/secrets. `value` is plaintext in RAM only — it is
/// sealed before touching the DB and never echoed back or logged.
#[derive(Debug, Deserialize)]
pub struct PutSecretRequest {
    pub name: String,
    pub scope: String,
    // Not Debug-printed anywhere; the struct derives Debug but axum
    // rejection paths never format the parsed body.
    pub value: String,
}

/// PUT /api/secrets (operator+; §12.2 write-only). Set or rotate:
/// an existing (name, scope) gets a version bump. Kicks a render pass
/// so affected devices' manifests pick up the new secrets_version
/// (§12.4 propagation).
pub async fn put_route(
    State(state): State<AppState>,
    identity: Identity,
    Json(body): Json<PutSecretRequest>,
) -> Response {
    if let Err(status) = crate::join_tokens::require_at_least(&state, &identity, Role::Operator) {
        return status.into_response();
    }
    if !valid_name(&body.name) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": "invalid secret name (alphanumeric, `-`, `_`, `.`; max 200)" })),
        )
            .into_response();
    }
    if !valid_scope(&body.scope) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "error": "invalid scope: fleet | class.<n> | region.<n> | site.<n> | device.<id> | reeve-internal"
            })),
        )
            .into_response();
    }
    let key = match vault_key(&state.cfg.data_dir) {
        Ok(k) => k,
        Err(e) => return internal_error(e),
    };
    let version = {
        let mut conn = state.db.lock().expect("db mutex poisoned");
        // One transaction: vault write + render-dirty flag (Law 3 — a
        // kill -9 before the render pass below leaves the flag, and
        // startup reconcile runs the propagating pass, §12.4).
        let result = (|| -> anyhow::Result<u64> {
            let tx = conn.transaction()?;
            let v = put(&tx, &key, &body.name, &body.scope, &body.value)?;
            mark_render_dirty(&tx)?;
            tx.commit()?;
            Ok(v)
        })();
        match result {
            Ok(v) => v,
            Err(e) => return internal_error(e),
        }
    }; // db lock dropped BEFORE the render pass (state.rs lock order)
    info!(name = %body.name, scope = %body.scope, version, "secret set");
    crate::render::render_all_logged(&state);
    Json(json!({ "name": body.name, "scope": body.scope, "version": version })).into_response()
}

/// DELETE /api/secrets/{scope}/{name} (operator+). 404 when absent.
/// Kicks a render pass: referencing apps' secrets_version changes
/// (their references stop resolving), so consumers are notified.
pub async fn delete_route(
    State(state): State<AppState>,
    identity: Identity,
    Path((scope, name)): Path<(String, String)>,
) -> Response {
    if let Err(status) = crate::join_tokens::require_at_least(&state, &identity, Role::Operator) {
        return status.into_response();
    }
    let existed = {
        let mut conn = state.db.lock().expect("db mutex poisoned");
        // Same transaction shape as put_route (Law 3).
        let result = (|| -> anyhow::Result<bool> {
            let tx = conn.transaction()?;
            let b = delete(&tx, &name, &scope)?;
            if b {
                mark_render_dirty(&tx)?;
            }
            tx.commit()?;
            Ok(b)
        })();
        match result {
            Ok(b) => b,
            Err(e) => return internal_error(e),
        }
    };
    if !existed {
        return StatusCode::NOT_FOUND.into_response();
    }
    info!(name = %name, scope = %scope, "secret deleted");
    crate::render::render_all_logged(&state);
    Json(json!({ "deleted": true })).into_response()
}

/// GET /api/secrets (viewer+): METADATA ONLY — name, scope, version,
/// created/rotated. Values are never readable through any route, by
/// anyone (§12.2 write-only after entry).
pub async fn list_route(State(state): State<AppState>, identity: Identity) -> Response {
    if let Err(status) = crate::join_tokens::require_at_least(&state, &identity, Role::Viewer) {
        return status.into_response();
    }
    let rows = {
        let conn = state.db.lock().expect("db mutex poisoned");
        match list(&conn) {
            Ok(r) => r,
            Err(e) => return internal_error(e),
        }
    };
    Json(json!({ "secrets": rows })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "on").unwrap();
        crate::db::migrate(&mut conn).unwrap();
        conn
    }

    const KEY: [u8; KEY_LEN] = [7u8; KEY_LEN];

    #[test]
    fn scope_and_name_grammar() {
        for ok in ["fleet", "class.hmi", "region.emea", "site.plant-a", "device.dev-1", "reeve-internal"] {
            assert!(valid_scope(ok), "{ok}");
        }
        for bad in ["", "class.", "site", "device.", "Fleet", "global"] {
            assert!(!valid_scope(bad), "{bad:?}");
        }

        assert!(valid_name("db-password"));
        assert!(valid_name("zot.upstream.password"));
        assert!(!valid_name(""));
        assert!(!valid_name("has space"));
        assert!(!valid_name("brace}"));
        assert!(!valid_name(&"x".repeat(201)));
    }

    /// AEAD round-trip through the vault + wrong-key failure: values
    /// at rest are ciphertext; a foreign keyfile cannot read them.
    #[test]
    fn put_resolve_roundtrip_and_wrong_key_fails() {
        let conn = test_db();
        assert_eq!(put(&conn, &KEY, "db-password", "fleet", "hunter2").unwrap(), 1);

        // At rest: ciphertext only, never the plaintext.
        let stored: Vec<u8> = conn
            .query_row("SELECT ciphertext FROM secrets", [], |r| r.get(0))
            .unwrap();
        assert!(
            !stored.windows(7).any(|w| w == b"hunter2"),
            "plaintext leaked into the DB"
        );

        let chain = device_chain("dev-1", None, None, None);
        let names = BTreeSet::from(["db-password".to_string()]);
        let got = resolve_values(&conn, &KEY, &chain, &names).unwrap();
        assert_eq!(got["db-password"].value, "hunter2");
        assert_eq!(got["db-password"].version, 1);

        let wrong = [8u8; KEY_LEN];
        assert!(
            resolve_values(&conn, &wrong, &chain, &names).is_err(),
            "wrong keyfile must fail loudly, not omit"
        );
    }

    /// §12.4: rotate bumps the version and re-seals; delete removes.
    #[test]
    fn rotate_bumps_version_delete_removes() {
        let conn = test_db();
        assert_eq!(put(&conn, &KEY, "s", "fleet", "v1").unwrap(), 1);
        assert_eq!(put(&conn, &KEY, "s", "fleet", "v2").unwrap(), 2);
        assert_eq!(put(&conn, &KEY, "s", "site.a", "v1").unwrap(), 1, "per-scope versions");

        let rows = list(&conn).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].version, 2);
        assert!(rows[0].rotated_at.is_some());
        assert!(rows[1].rotated_at.is_none());

        assert!(delete(&conn, "s", "site.a").unwrap());
        assert!(!delete(&conn, "s", "site.a").unwrap(), "idempotent");
        assert_eq!(list(&conn).unwrap().len(), 1);
    }

    /// §12.1 scope precedence: fleet -> class -> region -> site ->
    /// device, deeper wins — exactly like config merge.
    #[test]
    fn scope_precedence_deeper_wins() {
        let conn = test_db();
        for (scope, value) in [
            ("fleet", "from-fleet"),
            ("class.hmi", "from-class"),
            ("region.emea", "from-region"),
            ("site.plant-a", "from-site"),
            ("device.dev-1", "from-device"),
        ] {
            put(&conn, &KEY, "s", scope, value).unwrap();
        }
        let names = BTreeSet::from(["s".to_string()]);
        let cases: [(&[&str], &str); 5] = [
            (&["dev-1", "hmi", "emea", "plant-a"], "from-device"),
            (&["dev-2", "hmi", "emea", "plant-a"], "from-site"),
            (&["dev-2", "hmi", "emea", "plant-b"], "from-region"),
            (&["dev-2", "hmi", "apac", "plant-b"], "from-class"),
            (&["dev-2", "plc", "apac", "plant-b"], "from-fleet"),
        ];
        for (parts, expect) in cases {
            let chain = device_chain(parts[0], Some(parts[1]), Some(parts[2]), Some(parts[3]));
            let got = resolve_values(&conn, &KEY, &chain, &names).unwrap();
            assert_eq!(got["s"].value, expect, "chain {chain:?}");
        }
    }

    /// §12.6: another device's device-scoped secret and the reserved
    /// internal scope are invisible — omitted, not errored.
    #[test]
    fn device_isolation_and_internal_scope_unresolvable() {
        let conn = test_db();
        put(&conn, &KEY, "only-b", "device.dev-b", "b-secret").unwrap();
        put(&conn, &KEY, "op-secret", INTERNAL_SCOPE, "internal").unwrap();

        let chain = device_chain("dev-a", None, None, Some("plant-a"));
        let names = BTreeSet::from([
            "only-b".to_string(),
            "op-secret".to_string(),
            "never-existed".to_string(),
        ]);
        let got = resolve_values(&conn, &KEY, &chain, &names).unwrap();
        assert!(got.is_empty(), "nothing resolvable must mean an empty map: {got:?}");
    }

    #[test]
    fn internal_typed_getters() {
        let conn = test_db();
        assert_eq!(zot_upstream_credentials(&conn, &KEY).unwrap(), None);
        put(&conn, &KEY, "zot.upstream.username", INTERNAL_SCOPE, "u").unwrap();
        put(&conn, &KEY, "zot.upstream.password", INTERNAL_SCOPE, "p").unwrap();
        put(&conn, &KEY, "tier.hub.token", INTERNAL_SCOPE, "tok").unwrap();
        assert_eq!(
            zot_upstream_credentials(&conn, &KEY).unwrap(),
            Some(("u".to_string(), "p".to_string()))
        );
        assert_eq!(s3_credentials(&conn, &KEY).unwrap(), None);
        assert_eq!(tier_token(&conn, &KEY, "hub").unwrap(), Some("tok".to_string()));
        assert_eq!(tier_token(&conn, &KEY, "other").unwrap(), None);
    }

    #[test]
    fn ref_collection_matches_agent_parser() {
        let mut refs = BTreeSet::new();
        collect_secret_refs("a ${secret:x} b ${secret:y-2} ${secret:x} ${secret:z", &mut refs);
        assert_eq!(refs, BTreeSet::from(["x".to_string(), "y-2".to_string()]));
    }

    const DEPLOYMENT_YAML: &str = "\
apiVersion: application.margo.org/v1alpha1
kind: ApplicationDeployment
metadata:
  name: web-deploy
spec:
  applicationId: web
  deploymentProfile:
    type: docker-compose
    components:
      - name: web-stack
  parameters:
    databaseUrl:
      value: \"postgres://app:${secret:db-password}@db:5432/app\"
      targets:
        - pointer: ENV.DATABASE_URL
          components: []
    apiKey:
      value: \"${secret:api-key}\"
      targets:
        - pointer: ENV.API_KEY
          components: []
    logLevel:
      value: info
      targets:
        - pointer: ENV.LOG_LEVEL
          components: []
";

    fn file_set(entries: &[(&str, &str)]) -> FileSet {
        entries
            .iter()
            .map(|(p, c)| (p.to_string(), c.as_bytes().to_vec()))
            .collect()
    }

    /// The render hook (§12.4): only referencing apps get an entry;
    /// the hash moves on rotate and on create, and is stable otherwise.
    #[test]
    fn app_secrets_versions_hashes_only_referencing_apps() {
        let conn = test_db();
        let files = file_set(&[
            ("apps/web/deployment.yaml", DEPLOYMENT_YAML),
            ("apps/web/compose.yml", "services: {}\n"),
            (
                "apps/plain/deployment.yaml",
                "apiVersion: a\nkind: k\nmetadata: {name: p}\nspec:\n  applicationId: plain\n  deploymentProfile: {type: docker-compose, components: [{name: s}]}\n  parameters:\n    logLevel:\n      value: warn\n      targets: [{pointer: ENV.L, components: []}]\n",
            ),
            ("manifest.yaml", "deviceId: d\n"),
        ]);
        let chain = device_chain("dev-1", None, None, Some("plant-a"));

        // No secrets exist yet: web still gets a (stable) hash — its
        // references exist — so CREATING the secret later bumps it.
        let before = app_secrets_versions(&conn, &files, &chain).unwrap();
        assert_eq!(before.keys().collect::<Vec<_>>(), ["web"]);
        assert_eq!(
            before,
            app_secrets_versions(&conn, &files, &chain).unwrap(),
            "stable while the vault is unchanged"
        );

        put(&conn, &KEY, "db-password", "fleet", "hunter2").unwrap();
        let created = app_secrets_versions(&conn, &files, &chain).unwrap();
        assert_ne!(created["web"], before["web"], "create bumps the hash");

        put(&conn, &KEY, "db-password", "fleet", "sw0rdfish").unwrap();
        let rotated = app_secrets_versions(&conn, &files, &chain).unwrap();
        assert_ne!(rotated["web"], created["web"], "rotate bumps the hash");

        // An unrelated secret leaves it untouched.
        put(&conn, &KEY, "unrelated", "fleet", "x").unwrap();
        assert_eq!(app_secrets_versions(&conn, &files, &chain).unwrap()["web"], rotated["web"]);

        // Hash covers versions, not values — and never contains them.
        assert!(rotated["web"].starts_with("sha256:"));
    }

    #[test]
    fn secrets_version_hash_is_boundary_safe() {
        let a = BTreeMap::from([("ab".to_string(), 1u64)]);
        let b = BTreeMap::from([("a".to_string(), 1u64), ("b".to_string(), 1u64)]);
        assert_ne!(secrets_version_hash(&a), secrets_version_hash(&b));
        assert_ne!(
            secrets_version_hash(&BTreeMap::from([("a".to_string(), 1u64)])),
            secrets_version_hash(&BTreeMap::from([("a".to_string(), 2u64)])),
        );
    }
}

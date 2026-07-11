//! History and Undo (spec/reeve/11-fleet-model.md §11.5): the operator
//! view of the revision store. The store still records every change
//! attributably (D13), but the UI never picks a revision — it reads
//! **History** (who changed what, when) and clicks **Undo**, which
//! authors a NEW revision restoring the content as of before the change.
//!
//! §11.5 copy rule: "tree", "revision", "layer", "blame" and numeric
//! layer paths MUST NOT appear in operator-facing output. So each entry
//! carries a human `summary` ("deployed nginx to Site plant-a") derived
//! from the diff, and the detail lists changed apps + scopes, never raw
//! paths. The power-user raw surface stays at /api/tree/* (tree.rs).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use device_api::{Identity, Role};
use revision_store::{Change, Revision, RevisionId, RevisionStore, Stream};
use serde::{Deserialize, Serialize};

use crate::join_tokens::require_at_least;
use crate::scope::{Scope, scope_of_layer_dir};
use crate::state::AppState;
use crate::tree::{author_of, history, internal, store_err, unprocessable};

/// One `GET /api/history` entry (§11.5: who/what/when, no plumbing).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    /// Opaque change id (the revision id under the hood, but the UI
    /// treats it as a change handle — §11.5).
    pub id: i64,
    /// RFC 3339 timestamp.
    pub when: String,
    /// The author of the change.
    pub who: String,
    /// Human one-liner, e.g. `deployed nginx to Site plant-a`, else
    /// `config change`.
    pub summary: String,
}

/// One changed app+scope in a [`HistoryDetail`] (never a raw path).
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct HistoryChange {
    /// `deployed` | `undeployed` | `changed`.
    pub change: String,
    /// The app id.
    pub app: String,
    /// The scope the change touched.
    pub scope: Scope,
    /// Human scope phrasing (`Site plant-a`).
    pub scope_label: String,
}

/// `GET /api/history/{id}` body: the plain, path-free diff (§11.5).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct HistoryDetail {
    pub id: i64,
    pub when: String,
    pub who: String,
    pub summary: String,
    /// Changed apps + scopes. Empty when the change touched only
    /// non-app content (a vendored package) — see `otherChanges`.
    pub changes: Vec<HistoryChange>,
    /// Count of changed tree paths outside any `apps/<app>/` (package
    /// vendoring, etc.) — surfaced as a number, never as paths.
    pub other_changes: usize,
}

/// `POST /api/history/{id}/undo` body.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UndoResponse {
    /// The new change id that restored prior content.
    pub revision: i64,
    /// `false` => nothing to undo (content already matched).
    pub changed: bool,
    /// The change id the content was restored to (before the undone
    /// change); `null` when the undone change was the first.
    pub restored_to: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    pub limit: Option<usize>,
}

// --------------------------------------------------------------------
// Summary derivation (diff -> human phrasing)
// --------------------------------------------------------------------

/// A parsed app-level change: which app, which scope, what happened.
struct AppChange {
    change: Verb,
    app: String,
    scope: Scope,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Verb {
    Deployed,
    Undeployed,
    Changed,
}

impl Verb {
    fn word(self) -> &'static str {
        match self {
            Verb::Deployed => "deployed",
            Verb::Undeployed => "undeployed",
            Verb::Changed => "changed",
        }
    }
    /// Verb + preposition for a sentence ("deployed X to", "updated X on").
    fn phrase(self) -> (&'static str, &'static str) {
        match self {
            Verb::Deployed => ("deployed", "to"),
            Verb::Undeployed => ("undeployed", "from"),
            Verb::Changed => ("updated", "on"),
        }
    }
}

/// Split `layers/<dir>/apps/<app>/<rest>` -> `(dir, app, rest)`.
fn parse_app_path(path: &str) -> Option<(&str, &str, &str)> {
    let rest = path.strip_prefix("layers/")?;
    let (dir, rest) = rest.split_once('/')?;
    let rest = rest.strip_prefix("apps/")?;
    let (app, rest) = rest.split_once('/')?;
    Some((dir, app, rest))
}

/// The diff of `rev` against its parent (or the whole tree, all-added,
/// for the first revision — the store cannot diff against a nonexistent
/// parent).
fn revision_changes(
    store: &RevisionStore,
    rev: &Revision,
) -> Result<Vec<(String, ChangeKind)>, revision_store::Error> {
    let entries = match rev.parent {
        Some(parent) => store
            .diff(parent, rev.id)?
            .into_iter()
            .map(|e| {
                let kind = match e.change {
                    Change::Added { .. } => ChangeKind::Added,
                    Change::Removed { .. } => ChangeKind::Removed,
                    Change::Modified { .. } => ChangeKind::Modified,
                };
                (e.path, kind)
            })
            .collect(),
        None => store
            .tree_at(rev.id)?
            .into_keys()
            .map(|p| (p, ChangeKind::Added))
            .collect(),
    };
    Ok(entries)
}

#[derive(Clone, Copy)]
enum ChangeKind {
    Added,
    Removed,
    Modified,
}

/// Fold a revision's changed paths into app-level changes (§11.5:
/// apps + scopes) plus a count of other (non-app) changed paths.
fn app_changes(changes: &[(String, ChangeKind)]) -> (Vec<AppChange>, usize) {
    use std::collections::BTreeMap;
    // (dir, app) -> verb; app.yaml add/remove wins over a bare "changed".
    let mut acc: BTreeMap<(String, String), Verb> = BTreeMap::new();
    let mut other = 0usize;
    for (path, kind) in changes {
        match parse_app_path(path) {
            Some((dir, app, rest)) => {
                let key = (dir.to_string(), app.to_string());
                if rest == "app.yaml" {
                    // The presence toggle: authoring it deploys, removing
                    // it undeploys, editing it changes.
                    let verb = match kind {
                        ChangeKind::Added => Verb::Deployed,
                        ChangeKind::Removed => Verb::Undeployed,
                        ChangeKind::Modified => Verb::Changed,
                    };
                    acc.insert(key, verb);
                } else {
                    // params/files: a change unless a stronger app.yaml
                    // verb already classified this app.
                    acc.entry(key).or_insert(Verb::Changed);
                }
            }
            None => other += 1,
        }
    }
    let list = acc
        .into_iter()
        .filter_map(|((dir, app), change)| {
            scope_of_layer_dir(&dir).map(|scope| AppChange { change, app, scope })
        })
        .collect();
    (list, other)
}

/// One human sentence for a revision.
fn summarize(store: &RevisionStore, rev: &Revision) -> String {
    let changes = match revision_changes(store, rev) {
        Ok(c) => c,
        Err(_) => return "config change".to_string(),
    };
    let (apps, other) = app_changes(&changes);
    phrase_summary(&apps, other)
}

/// Turn app changes into the summary line (shared by list + detail).
fn phrase_summary(apps: &[AppChange], other: usize) -> String {
    if apps.is_empty() {
        return "config change".to_string();
    }
    // N device layers, one app, one verb => "deployed nginx to 3 devices".
    let all_devices = apps.iter().all(|c| matches!(c.scope, Scope::Devices { .. }));
    let same_app = apps.windows(2).all(|w| w[0].app == w[1].app);
    let same_verb = apps.windows(2).all(|w| w[0].change == w[1].change);
    if apps.len() > 1 && all_devices && same_app && same_verb {
        let (verb, prep) = apps[0].change.phrase();
        return format!("{verb} {} {prep} {} devices", apps[0].app, apps.len());
    }
    if apps.len() == 1 {
        let c = &apps[0];
        let (verb, prep) = c.change.phrase();
        return format!("{verb} {} {prep} {}", c.app, c.scope.label());
    }
    // Mixed: count, mention the first, note the rest.
    let _ = other;
    let extra = apps.len() - 1;
    let c = &apps[0];
    format!("{} {} and {extra} more change(s)", c.change.word(), c.app)
}

// --------------------------------------------------------------------
// Routes
// --------------------------------------------------------------------

/// GET /api/history[?limit=N] (viewer+) — the change log, newest first,
/// each with a human summary (§11.5).
#[utoipa::path(
    get,
    path = "/api/history",
    tag = "history",
    operation_id = "history_list",
    params(("limit" = Option<usize>, Query, description = "Max entries; default 100, cap 1000")),
    responses(
        (status = 200, description = "Change history, newest first", body = Vec<HistoryEntry>),
        (status = 401, description = "Unauthenticated"),
        (status = 403, description = "Below viewer role"),
    ),
)]
pub async fn list(
    State(state): State<AppState>,
    identity: Identity,
    Query(q): Query<HistoryQuery>,
) -> Response {
    if let Err(status) = require_at_least(&state, &identity, Role::Viewer) {
        return status.into_response();
    }
    let limit = q.limit.unwrap_or(100).min(1000);
    let store = state.revisions.lock().expect("revisions mutex poisoned");
    match history(&store, limit) {
        Ok(revs) => {
            let out: Vec<HistoryEntry> = revs
                .into_iter()
                .map(|rev| {
                    let summary = summarize(&store, &rev);
                    HistoryEntry {
                        id: rev.id,
                        when: rev.created_at,
                        who: rev.author,
                        summary,
                    }
                })
                .collect();
            Json(out).into_response()
        }
        Err(e) => store_err(e),
    }
}

/// GET /api/history/{id} (viewer+) — the plain diff: changed apps +
/// scopes, never raw layer paths (§11.5).
#[utoipa::path(
    get,
    path = "/api/history/{id}",
    tag = "history",
    operation_id = "history_detail",
    params(("id" = i64, Path, description = "Change id")),
    responses(
        (status = 200, description = "Change detail: summary + changed apps/scopes", body = HistoryDetail),
        (status = 401, description = "Unauthenticated"),
        (status = 403, description = "Below viewer role"),
        (status = 404, description = "Unknown change", body = device_api::ErrorBody),
    ),
)]
pub async fn detail(
    State(state): State<AppState>,
    identity: Identity,
    Path(id): Path<RevisionId>,
) -> Response {
    if let Err(status) = require_at_least(&state, &identity, Role::Viewer) {
        return status.into_response();
    }
    let store = state.revisions.lock().expect("revisions mutex poisoned");
    let rev = match store.revision(id) {
        Ok(r) => r,
        Err(e) => return store_err(e),
    };
    let changes = match revision_changes(&store, &rev) {
        Ok(c) => c,
        Err(e) => return store_err(e),
    };
    let (apps, other) = app_changes(&changes);
    let summary = phrase_summary(&apps, other);
    let changes_out: Vec<HistoryChange> = apps
        .into_iter()
        .map(|c| HistoryChange {
            change: c.change.word().to_string(),
            app: c.app,
            scope_label: c.scope.label(),
            scope: c.scope,
        })
        .collect();
    Json(HistoryDetail {
        id: rev.id,
        when: rev.created_at,
        who: rev.author,
        summary,
        changes: changes_out,
        other_changes: other,
    })
    .into_response()
}

/// POST /api/history/{id}/undo (operator+) — author a NEW revision that
/// restores the whole tree to its content as of BEFORE change `{id}`
/// (§11.5 Undo). manifestVersion still only ever climbs (content may
/// revert; the version counter never does — §11.5 note). Only local
/// changes are undoable; upstream (synced) changes are refused.
#[utoipa::path(
    post,
    path = "/api/history/{id}/undo",
    tag = "history",
    params(("id" = i64, Path, description = "Change id to undo")),
    responses(
        (status = 200, description = "Undone — a new change restored prior content", body = UndoResponse),
        (status = 401, description = "Unauthenticated"),
        (status = 403, description = "Below operator role"),
        (status = 404, description = "Unknown change", body = device_api::ErrorBody),
        (status = 422, description = "Not an undoable (local) change", body = device_api::ErrorBody),
    ),
)]
pub async fn undo(
    State(state): State<AppState>,
    identity: Identity,
    Path(id): Path<RevisionId>,
) -> Response {
    if let Err(status) = require_at_least(&state, &identity, Role::Operator) {
        return status.into_response();
    }
    let author = author_of(&identity);
    let committed = {
        let mut store = state.revisions.lock().expect("revisions mutex poisoned");
        let rev = match store.revision(id) {
            Ok(r) => r,
            Err(e) => return store_err(e),
        };
        // Upstream revisions are a read-only mirror of the parent tier
        // (federation §8.2) — undoing one here is meaningless and would
        // desync; refuse it.
        if rev.stream != Stream::Local {
            return unprocessable("only local changes can be undone");
        }
        let before = rev.parent;
        // Whole-tree snapshot as of `before` (empty when `id` was the
        // first change): store.commit sets the new revision's tree to
        // exactly this manifest, so committing it restores prior content.
        let manifest: Vec<(String, Vec<u8>)> = match before {
            Some(p) => {
                let tree = match store.tree_at(p) {
                    Ok(t) => t,
                    Err(e) => return store_err(e),
                };
                let mut files = Vec::with_capacity(tree.len());
                for (path, digest) in tree {
                    match store.blob(&digest) {
                        Ok(Some(bytes)) => files.push((path, bytes)),
                        Ok(None) => {
                            return internal(format!("missing blob {digest} for {path}"));
                        }
                        Err(e) => return store_err(e),
                    }
                }
                files
            }
            None => Vec::new(),
        };
        let message = format!("undo change #{id}");
        let head = match store.head(Stream::Local) {
            Ok(h) => h,
            Err(e) => return store_err(e),
        };
        match store.commit(manifest, &author, &message, Stream::Local) {
            Ok(new_id) => Ok((new_id, head != Some(new_id), before)),
            Err(e) => Err(e),
        }
    };
    match committed {
        Ok((revision, changed, restored_to)) => {
            if changed {
                crate::render::render_all_logged(&state);
            }
            Json(UndoResponse {
                revision,
                changed,
                restored_to,
            })
            .into_response()
        }
        Err(e) => store_err(e),
    }
}

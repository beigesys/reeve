//! Canonical location groups + fleet->site containment (REV-010
//! amendment; spec/reeve/11-fleet-model.md §11.1/§11.3).
//!
//! The fleet->site hierarchy is a real CONTAINMENT TREE: a Site belongs
//! to exactly one Fleet. This module is the canonical store of that tree
//! (`location_groups`, V11) plus the operator API to manage it, and the
//! validation both device assignment (devices.rs) and enrollment
//! pre-assign (enroll.rs) run against so a device can never be assigned a
//! site that does not belong to its fleet (the "mixed" bug this fixes).
//!
//! Device-type is NOT a group — it stays the orthogonal free column
//! `devices."type"` (a hardware class applies at any site). Tags stay
//! free. Only fleet/site are contained.
//!
//! Contract split (recorded here):
//! - Interactive assignment (PATCH /api/devices) is STRICT: it refuses
//!   (422) a site/fleet that is not already a group. New locations are
//!   created explicitly via POST /api/groups — the canonical surface.
//! - Automated enrollment pre-assign AUTO-PROVISIONS the token's
//!   fleet/site groups ([`ensure_groups`]) so a device join never fails
//!   over a missing group, while STILL creating the site UNDER the
//!   token's fleet (containment preserved, never orphaned).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use device_api::{Identity, Role};
use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::warn;
use utoipa::ToSchema;

use crate::db::now_secs;
use crate::join_tokens::require_at_least;
use crate::state::AppState;

fn internal(e: impl std::fmt::Display) -> Response {
    warn!(error = %e, "groups route internal error");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

fn unprocessable(msg: impl Into<String>) -> Response {
    (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({ "error": msg.into() }))).into_response()
}

fn conflict(msg: impl Into<String>) -> Response {
    (StatusCode::CONFLICT, Json(json!({ "error": msg.into() }))).into_response()
}

fn not_found(msg: impl Into<String>) -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": msg.into() }))).into_response()
}

/// True when a rusqlite error is a UNIQUE/CHECK constraint violation
/// (a duplicate group name => 409, not a 500).
fn is_constraint(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

// ---------------------------------------------------------------------
// Name grammar
// ---------------------------------------------------------------------

/// Validate a group name. A fleet/site name becomes the label of a merge
/// layer (`10-fleet.<name>` / `20-site.<name>`, §11.1), so it must satisfy
/// the D11 layer-label grammar: 1..=128 chars of `[A-Za-z0-9._-]`,
/// starting with an alphanumeric and not ending with `.`.
pub fn validate_group_name(name: &str) -> Result<(), String> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 128 {
        return Err("group name must be 1..=128 characters".to_string());
    }
    if !bytes[0].is_ascii_alphanumeric() {
        return Err(format!("group name `{name}` must start with an alphanumeric"));
    }
    if name.ends_with('.') {
        return Err(format!("group name `{name}` must not end with `.`"));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
    {
        return Err(format!("group name `{name}`: illegal character `{bad}`"));
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Store helpers — reused by devices.rs (validation) and enroll.rs
// ---------------------------------------------------------------------

/// The group_id of a fleet by name, if it exists.
pub fn fleet_group_id(conn: &Connection, name: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT group_id FROM location_groups WHERE kind = 'fleet' AND name = ?1",
        params![name],
        |r| r.get(0),
    )
    .optional()
}

/// True if a site named `site` exists under the fleet with id `fleet_id`.
fn site_exists_under(conn: &Connection, fleet_id: i64, site: &str) -> rusqlite::Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM location_groups
             WHERE kind = 'site' AND parent_id = ?1 AND name = ?2",
            params![fleet_id, site],
            |r| r.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// Validate a proposed `(fleet, site)` device assignment against the
/// containment tree (§11.1). Empty strings are treated as unset. Returns
/// `Ok(Ok(()))` when valid, `Ok(Err(msg))` for a containment violation
/// (=> 422), and `Err` only on a DB failure (=> 500).
///
/// Rules (STRICT — the interactive path never free-adds a group):
/// - a site requires a fleet;
/// - the fleet must be an existing fleet group;
/// - the site must be an existing site group UNDER that fleet.
pub fn validate_location(
    conn: &Connection,
    fleet: Option<&str>,
    site: Option<&str>,
) -> rusqlite::Result<Result<(), String>> {
    let fleet = fleet.filter(|s| !s.is_empty());
    let site = site.filter(|s| !s.is_empty());
    match (fleet, site) {
        // Nothing set, or fleet-only: a fleet must be a known group.
        (Some(f), None) => {
            if fleet_group_id(conn, f)?.is_none() {
                return Ok(Err(format!(
                    "unknown fleet `{f}` — create it first via POST /api/groups"
                )));
            }
            Ok(Ok(()))
        }
        (None, None) => Ok(Ok(())),
        // A site with no fleet has nothing to be contained by.
        (None, Some(_)) => Ok(Err(
            "assigning a site requires a fleet (a site belongs to exactly one fleet, §11.1)"
                .to_string(),
        )),
        // Both set: the site must live under this exact fleet.
        (Some(f), Some(s)) => {
            let Some(fid) = fleet_group_id(conn, f)? else {
                return Ok(Err(format!(
                    "unknown fleet `{f}` — create it first via POST /api/groups"
                )));
            };
            if !site_exists_under(conn, fid, s)? {
                return Ok(Err(format!(
                    "site `{s}` does not belong to fleet `{f}` — pick a site that exists \
                     under it, or create it via POST /api/groups (containment, §11.1)"
                )));
            }
            Ok(Ok(()))
        }
    }
}

/// Auto-provision the groups an ENROLLMENT pre-assign needs so a device
/// join never fails over a missing group, while keeping containment: the
/// fleet group is created if absent, and the site is created UNDER that
/// fleet. A site with no fleet cannot be contained and is skipped (left as
/// a free-text column — a misconfigured token, mirrored from the V11
/// backfill). Idempotent (`INSERT OR IGNORE`). Runs inside the caller's
/// enrollment transaction.
pub fn ensure_groups(
    conn: &Connection,
    fleet: Option<&str>,
    site: Option<&str>,
) -> rusqlite::Result<()> {
    let fleet = fleet.filter(|s| !s.is_empty());
    let site = site.filter(|s| !s.is_empty());
    let Some(f) = fleet else { return Ok(()) };
    let now = now_secs();
    conn.execute(
        "INSERT OR IGNORE INTO location_groups (kind, name, parent_id, created_at)
         VALUES ('fleet', ?1, NULL, ?2)",
        params![f, now],
    )?;
    if let Some(s) = site {
        let fid: i64 = conn.query_row(
            "SELECT group_id FROM location_groups WHERE kind = 'fleet' AND name = ?1",
            params![f],
            |r| r.get(0),
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO location_groups (kind, name, parent_id, created_at)
             VALUES ('site', ?1, ?2, ?3)",
            params![s, fid, now],
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------

/// A group kind (§11.1): only fleet and site are contained groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum GroupKind {
    Fleet,
    Site,
}

/// One site under a fleet in the [`GroupTree`].
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SiteNode {
    pub id: i64,
    pub name: String,
}

/// One fleet with its contained sites.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FleetNode {
    pub id: i64,
    pub name: String,
    /// Sites contained by this fleet, ordered by name.
    pub sites: Vec<SiteNode>,
}

/// `GET /api/groups` body: the fleet->site containment tree. A scoped
/// read (`?fleet=<name>`) returns the same shape with only that fleet's
/// subtree in `fleets`.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GroupTree {
    pub fleets: Vec<FleetNode>,
}

/// A single created/renamed group.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GroupNode {
    pub id: i64,
    pub kind: GroupKind,
    pub name: String,
    /// The parent fleet's id for a site; `null` for a fleet.
    pub parent_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct GroupsQuery {
    /// Optional filter; with `fleet`, scopes the read to that fleet's
    /// children. Accepted values: `fleet`, `site`.
    pub kind: Option<String>,
    /// Scoped-children read: return only this fleet's subtree (its sites).
    pub fleet: Option<String>,
}

// ---------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------

/// Build the full fleet->site tree (or one fleet's subtree if `only_fleet`
/// is set). Returns `None` when `only_fleet` names an unknown fleet.
fn load_tree(conn: &Connection, only_fleet: Option<&str>) -> rusqlite::Result<Option<GroupTree>> {
    let mut fleet_stmt = if only_fleet.is_some() {
        conn.prepare(
            "SELECT group_id, name FROM location_groups
             WHERE kind = 'fleet' AND name = ?1 ORDER BY name",
        )?
    } else {
        conn.prepare(
            "SELECT group_id, name FROM location_groups
             WHERE kind = 'fleet' ORDER BY name",
        )?
    };
    let fleet_rows: Vec<(i64, String)> = if let Some(f) = only_fleet {
        fleet_stmt
            .query_map(params![f], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?
    } else {
        fleet_stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?
    };

    if only_fleet.is_some() && fleet_rows.is_empty() {
        return Ok(None);
    }

    let mut site_stmt = conn.prepare(
        "SELECT group_id, name FROM location_groups
         WHERE kind = 'site' AND parent_id = ?1 ORDER BY name",
    )?;
    let mut fleets = Vec::with_capacity(fleet_rows.len());
    for (id, name) in fleet_rows {
        let sites: Vec<SiteNode> = site_stmt
            .query_map(params![id], |r| {
                Ok(SiteNode {
                    id: r.get(0)?,
                    name: r.get(1)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        fleets.push(FleetNode { id, name, sites });
    }
    Ok(Some(GroupTree { fleets }))
}

/// GET /api/groups (viewer+) — the fleet->site containment tree.
/// `?fleet=<name>` (optionally `&kind=site`) scopes the read to one
/// fleet's children (lazy drill-down).
#[utoipa::path(
    get,
    path = "/api/groups",
    tag = "groups",
    operation_id = "groups_list",
    params(
        ("kind" = Option<String>, Query, description = "Optional filter (`fleet`|`site`); with `fleet` requests that fleet's sites"),
        ("fleet" = Option<String>, Query, description = "Scoped-children read: only this fleet's subtree"),
    ),
    responses(
        (status = 200, description = "The fleet->site containment tree (or one fleet's subtree)", body = GroupTree),
        (status = 401, description = "Unauthenticated"),
        (status = 403, description = "Below viewer role"),
        (status = 404, description = "Unknown fleet (scoped read)", body = device_api::ErrorBody),
    ),
)]
pub async fn list(
    State(state): State<AppState>,
    identity: Identity,
    Query(q): Query<GroupsQuery>,
) -> Response {
    if let Err(status) = require_at_least(&state, &identity, Role::Viewer) {
        return status.into_response();
    }
    let conn = state.db.lock().expect("db mutex poisoned");
    match load_tree(&conn, q.fleet.as_deref()) {
        Ok(Some(tree)) => Json(tree).into_response(),
        Ok(None) => not_found(format!("unknown fleet `{}`", q.fleet.unwrap_or_default())),
        Err(e) => internal(e),
    }
}

// ---------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------

/// `POST /api/groups` body: create a fleet (no parent) or a site (parent =
/// an existing fleet's id).
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateGroupRequest {
    pub kind: GroupKind,
    pub name: String,
    /// Required for a site (the containing fleet's id); MUST be absent for
    /// a fleet.
    pub parent_id: Option<i64>,
}

/// POST /api/groups (operator+) — create a fleet or a site. A site
/// requires `parentId` = an existing fleet; a fleet must have none.
/// Duplicate name (a fleet globally, a site within its fleet) => 409.
#[utoipa::path(
    post,
    path = "/api/groups",
    tag = "groups",
    operation_id = "groups_create",
    request_body = CreateGroupRequest,
    responses(
        (status = 201, description = "Group created", body = GroupNode),
        (status = 401, description = "Unauthenticated"),
        (status = 403, description = "Below operator role"),
        (status = 409, description = "Duplicate name (fleet globally, or site within its fleet)", body = device_api::ErrorBody),
        (status = 422, description = "Invalid name, or a site without/with a bad parent fleet, or a fleet with a parent", body = device_api::ErrorBody),
    ),
)]
pub async fn create(
    State(state): State<AppState>,
    identity: Identity,
    Json(body): Json<CreateGroupRequest>,
) -> Response {
    if let Err(status) = require_at_least(&state, &identity, Role::Operator) {
        return status.into_response();
    }
    if let Err(msg) = validate_group_name(&body.name) {
        return unprocessable(msg);
    }
    let now = now_secs();
    let conn = state.db.lock().expect("db mutex poisoned");

    let parent_id = match body.kind {
        GroupKind::Fleet => {
            if body.parent_id.is_some() {
                return unprocessable("a fleet is top-level and must not have a parent");
            }
            None
        }
        GroupKind::Site => {
            let Some(pid) = body.parent_id else {
                return unprocessable("a site requires `parentId` = an existing fleet");
            };
            let is_fleet: Option<i64> = match conn
                .query_row(
                    "SELECT 1 FROM location_groups WHERE group_id = ?1 AND kind = 'fleet'",
                    params![pid],
                    |r| r.get(0),
                )
                .optional()
            {
                Ok(v) => v,
                Err(e) => return internal(e),
            };
            if is_fleet.is_none() {
                return unprocessable(format!("parent `{pid}` is not an existing fleet"));
            }
            Some(pid)
        }
    };

    let kind = match body.kind {
        GroupKind::Fleet => "fleet",
        GroupKind::Site => "site",
    };
    let res = conn.execute(
        "INSERT INTO location_groups (kind, name, parent_id, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![kind, body.name, parent_id, now],
    );
    match res {
        Ok(_) => {
            let id = conn.last_insert_rowid();
            (
                StatusCode::CREATED,
                Json(GroupNode {
                    id,
                    kind: body.kind,
                    name: body.name,
                    parent_id,
                }),
            )
                .into_response()
        }
        Err(e) if is_constraint(&e) => conflict(format!(
            "a {kind} named `{}` already exists in this scope",
            body.name
        )),
        Err(e) => internal(e),
    }
}

/// One loaded group row (for rename/delete).
struct GroupRow {
    kind: String,
    name: String,
    parent_id: Option<i64>,
}

fn load_group(conn: &Connection, id: i64) -> rusqlite::Result<Option<GroupRow>> {
    conn.query_row(
        "SELECT kind, name, parent_id FROM location_groups WHERE group_id = ?1",
        params![id],
        |r| {
            Ok(GroupRow {
                kind: r.get(0)?,
                name: r.get(1)?,
                parent_id: r.get(2)?,
            })
        },
    )
    .optional()
}

/// Whether a group is referenced (by a device assignment, or — for a
/// fleet — has child sites). In use => rename/delete is refused (409):
/// the name columns on `devices` remain the source of truth, so we never
/// change a name out from under a live assignment.
fn in_use(conn: &Connection, g: &GroupRow) -> rusqlite::Result<bool> {
    match g.kind.as_str() {
        "fleet" => {
            let n: i64 = conn.query_row(
                "SELECT
                   (SELECT count(*) FROM devices WHERE fleet = ?1) +
                   (SELECT count(*) FROM location_groups WHERE kind = 'site' AND parent_id =
                       (SELECT group_id FROM location_groups WHERE kind = 'fleet' AND name = ?1))",
                params![g.name],
                |r| r.get(0),
            )?;
            Ok(n > 0)
        }
        _ => {
            // Site: a device references it iff its site name matches AND it
            // sits under this site's fleet (site names are per-fleet).
            let fleet_name: Option<String> = conn
                .query_row(
                    "SELECT name FROM location_groups WHERE group_id = ?1",
                    params![g.parent_id],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(fleet_name) = fleet_name else {
                return Ok(false);
            };
            let n: i64 = conn.query_row(
                "SELECT count(*) FROM devices WHERE site = ?1 AND fleet = ?2",
                params![g.name, fleet_name],
                |r| r.get(0),
            )?;
            Ok(n > 0)
        }
    }
}

/// `PATCH /api/groups/{id}` body: rename a group.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RenameGroupRequest {
    pub name: String,
}

/// PATCH /api/groups/{id} (operator+) — rename a group. Refused (409) when
/// the group is still referenced by a device (or, for a fleet, still has
/// child sites): the device name columns stay valid because a live name is
/// never changed out from under them — reassign first. A rename that
/// collides with a sibling is also 409.
#[utoipa::path(
    patch,
    path = "/api/groups/{id}",
    tag = "groups",
    operation_id = "groups_rename",
    params(("id" = i64, Path, description = "Group id")),
    request_body = RenameGroupRequest,
    responses(
        (status = 200, description = "Renamed group", body = GroupNode),
        (status = 401, description = "Unauthenticated"),
        (status = 403, description = "Below operator role"),
        (status = 404, description = "Unknown group", body = device_api::ErrorBody),
        (status = 409, description = "In use (reassign first), or duplicate name", body = device_api::ErrorBody),
        (status = 422, description = "Invalid name", body = device_api::ErrorBody),
    ),
)]
pub async fn rename(
    State(state): State<AppState>,
    identity: Identity,
    Path(id): Path<i64>,
    Json(body): Json<RenameGroupRequest>,
) -> Response {
    if let Err(status) = require_at_least(&state, &identity, Role::Operator) {
        return status.into_response();
    }
    if let Err(msg) = validate_group_name(&body.name) {
        return unprocessable(msg);
    }
    let conn = state.db.lock().expect("db mutex poisoned");
    let g = match load_group(&conn, id) {
        Ok(Some(g)) => g,
        Ok(None) => return not_found(format!("unknown group `{id}`")),
        Err(e) => return internal(e),
    };
    match in_use(&conn, &g) {
        Ok(true) => {
            return conflict(format!(
                "{} `{}` is in use — reassign its devices{} before renaming",
                g.kind,
                g.name,
                if g.kind == "fleet" { " and remove its sites" } else { "" }
            ));
        }
        Ok(false) => {}
        Err(e) => return internal(e),
    }
    let res = conn.execute(
        "UPDATE location_groups SET name = ?1 WHERE group_id = ?2",
        params![body.name, id],
    );
    match res {
        Ok(_) => {
            let kind = if g.kind == "fleet" { GroupKind::Fleet } else { GroupKind::Site };
            Json(GroupNode {
                id,
                kind,
                name: body.name,
                parent_id: g.parent_id,
            })
            .into_response()
        }
        Err(e) if is_constraint(&e) => {
            conflict(format!("a {} named `{}` already exists in this scope", g.kind, body.name))
        }
        Err(e) => internal(e),
    }
}

/// DELETE /api/groups/{id} (operator+) — delete a group. Refused (409)
/// when it is still referenced by a device (or, for a fleet, still has
/// child sites): refuse is safer than a silent cascade that would orphan
/// live assignments — reassign/remove first.
#[utoipa::path(
    delete,
    path = "/api/groups/{id}",
    tag = "groups",
    operation_id = "groups_delete",
    params(("id" = i64, Path, description = "Group id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Unauthenticated"),
        (status = 403, description = "Below operator role"),
        (status = 404, description = "Unknown group", body = device_api::ErrorBody),
        (status = 409, description = "In use — reassign devices (and remove child sites) first", body = device_api::ErrorBody),
    ),
)]
pub async fn delete(
    State(state): State<AppState>,
    identity: Identity,
    Path(id): Path<i64>,
) -> Response {
    if let Err(status) = require_at_least(&state, &identity, Role::Operator) {
        return status.into_response();
    }
    let conn = state.db.lock().expect("db mutex poisoned");
    let g = match load_group(&conn, id) {
        Ok(Some(g)) => g,
        Ok(None) => return not_found(format!("unknown group `{id}`")),
        Err(e) => return internal(e),
    };
    match in_use(&conn, &g) {
        Ok(true) => {
            return conflict(format!(
                "{} `{}` is in use — reassign its devices{} before deleting",
                g.kind,
                g.name,
                if g.kind == "fleet" { " and remove its sites" } else { "" }
            ));
        }
        Ok(false) => {}
        Err(e) => return internal(e),
    }
    match conn.execute("DELETE FROM location_groups WHERE group_id = ?1", params![id]) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        // FK RESTRICT backstop (a fleet with sites racing the in_use check).
        Err(e) if is_constraint(&e) => {
            conflict(format!("{} `{}` still has dependents", g.kind, g.name))
        }
        Err(e) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        c.pragma_update(None, "foreign_keys", "on").unwrap();
        crate::db::migrate(&mut c).unwrap();
        c
    }

    fn mk_fleet(c: &Connection, name: &str) -> i64 {
        c.execute(
            "INSERT INTO location_groups (kind, name, parent_id) VALUES ('fleet', ?1, NULL)",
            params![name],
        )
        .unwrap();
        c.last_insert_rowid()
    }

    fn mk_site(c: &Connection, name: &str, fleet_id: i64) -> i64 {
        c.execute(
            "INSERT INTO location_groups (kind, name, parent_id) VALUES ('site', ?1, ?2)",
            params![name, fleet_id],
        )
        .unwrap();
        c.last_insert_rowid()
    }

    #[test]
    fn name_grammar() {
        assert!(validate_group_name("north").is_ok());
        assert!(validate_group_name("plant-a.1").is_ok());
        assert!(validate_group_name("").is_err());
        assert!(validate_group_name(".bad").is_err());
        assert!(validate_group_name("bad.").is_err());
        assert!(validate_group_name("has/slash").is_err());
    }

    #[test]
    fn containment_validation() {
        let c = conn();
        let north = mk_fleet(&c, "north");
        mk_site(&c, "plant-a", north);
        mk_fleet(&c, "south");

        // valid: site under its fleet
        assert!(validate_location(&c, Some("north"), Some("plant-a")).unwrap().is_ok());
        // fleet-only, known fleet
        assert!(validate_location(&c, Some("north"), None).unwrap().is_ok());
        // wrong fleet: plant-a is under north, not south
        assert!(validate_location(&c, Some("south"), Some("plant-a")).unwrap().is_err());
        // site with no fleet
        assert!(validate_location(&c, None, Some("plant-a")).unwrap().is_err());
        // unknown fleet
        assert!(validate_location(&c, Some("ghost"), None).unwrap().is_err());
        // nothing set
        assert!(validate_location(&c, None, None).unwrap().is_ok());
    }

    #[test]
    fn site_name_unique_per_fleet_not_global() {
        let c = conn();
        let north = mk_fleet(&c, "north");
        let south = mk_fleet(&c, "south");
        mk_site(&c, "plant-a", north);
        // same site name under a DIFFERENT fleet is allowed
        mk_site(&c, "plant-a", south);
        // duplicate under the SAME fleet violates the partial unique index
        assert!(is_constraint(
            &c.execute(
                "INSERT INTO location_groups (kind, name, parent_id) VALUES ('site','plant-a',?1)",
                params![north],
            )
            .unwrap_err()
        ));
    }

    #[test]
    fn fleet_name_globally_unique() {
        let c = conn();
        mk_fleet(&c, "north");
        assert!(is_constraint(
            &c.execute(
                "INSERT INTO location_groups (kind, name, parent_id) VALUES ('fleet','north',NULL)",
                [],
            )
            .unwrap_err()
        ));
    }

    #[test]
    fn ensure_groups_auto_provisions_under_fleet() {
        let c = conn();
        ensure_groups(&c, Some("north"), Some("plant-a")).unwrap();
        // idempotent
        ensure_groups(&c, Some("north"), Some("plant-a")).unwrap();
        let fid = fleet_group_id(&c, "north").unwrap().unwrap();
        assert!(site_exists_under(&c, fid, "plant-a").unwrap());
        // a site with no fleet is skipped (nothing to contain it)
        ensure_groups(&c, None, Some("orphan")).unwrap();
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM location_groups WHERE name = 'orphan'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn in_use_detection() {
        let c = conn();
        let north = mk_fleet(&c, "north");
        mk_site(&c, "plant-a", north);
        c.execute(
            "INSERT INTO devices (device_id, enrolled_at, fleet, site)
             VALUES ('d1', 0, 'north', 'plant-a')",
            [],
        )
        .unwrap();
        let fleet = load_group(&c, north).unwrap().unwrap();
        assert!(in_use(&c, &fleet).unwrap(), "fleet has a device (and a site)");
        let site_id: i64 = c
            .query_row(
                "SELECT group_id FROM location_groups WHERE kind='site' AND name='plant-a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let site = load_group(&c, site_id).unwrap().unwrap();
        assert!(in_use(&c, &site).unwrap(), "site has a device");
    }
}

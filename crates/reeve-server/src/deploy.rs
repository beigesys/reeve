//! Deploy-to-scope (spec/reeve/11-fleet-model.md §11.4): the operator
//! deploys a **stack** (a vendored package + selected profile) to a
//! **scope** (§11.4), never by editing a numbered layer.
//!
//! Under the hood every deploy is a normal authoring commit (D14)
//! through the SAME ownership/delegation gate as PUT /api/tree/layers
//! (tree.rs) — the operator sees "deploy nginx to Site plant-a", the
//! store sees one revision writing `layers/20-site.plant-a/apps/nginx/
//! app.yaml` (§11.4). Undeploy is the same call removing the app dir
//! from the scope (§11.4: "the same call removing the app from the
//! scope") — no `enabled:false` tombstone, symmetric with deploy
//! (DECISION, D11: an app is desired iff a layer in the chain defines
//! it; removing the definition from the scope is exactly "undeploy from
//! that scope", and it leaves any higher-scope deploy untouched).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use device_api::{Identity, Role};
use revision_store::Stream;
use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::join_tokens::require_at_least;
use crate::scope::Scope;
use crate::state::AppState;
use crate::tree::{
    Replacement, check_not_delegated, commit_replacements, internal, unprocessable,
    validate_package_segment, validate_rel_path,
};

/// The stack to deploy: a vendored package (`packages/<package>/
/// <version>/`, D11) and an optional deployment profile. `name` is the
/// app's dir under the scope layer (`apps/<name>/`) — defaults to the
/// package name, so `{package, version}` is the minimal stack.
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct StackRef {
    /// App id in the layer (`apps/<name>/`). Absent => the package name.
    #[serde(default)]
    pub name: Option<String>,
    /// Vendored package name (`packages/<package>/`).
    pub package: String,
    /// Vendored package version.
    pub version: String,
    /// Deployment profile id (Margo `deploymentProfiles[].id`); absent =>
    /// the package's sole/only-compose profile is selected at render.
    #[serde(default)]
    pub profile: Option<String>,
}

impl StackRef {
    /// The app dir name (`apps/<app>/`) — explicit `name`, else package.
    fn app(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.package)
    }
}

/// `POST /api/deploy` / `POST /api/undeploy` body (§11.4).
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeployRequest {
    pub stack: StackRef,
    pub scope: Scope,
}

/// The `app.yaml` a deploy authors (D11 app source: package ref +
/// `enabled: true` + optional profile). Serialized with serde so
/// numeric-looking versions/profiles are quoted (render requires
/// `package.version` be a string — desired-state render.rs).
#[derive(Serialize)]
struct AppYaml<'a> {
    enabled: bool,
    package: PackageRef<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<&'a str>,
}

#[derive(Serialize)]
struct PackageRef<'a> {
    name: &'a str,
    version: &'a str,
}

/// Response of both deploy and undeploy: the authoring outcome plus the
/// human framing (§11.5 — never a raw layer path).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeployResponse {
    pub revision: i64,
    /// `false` => identical content, no new revision (D14 idempotence).
    pub changed: bool,
    /// The deployed/undeployed app id.
    pub app: String,
    /// Human scope phrasing, e.g. `Site plant-a` (§11.5).
    pub scope: String,
}

/// Shared body for deploy (`enabled` app.yaml) and undeploy (remove the
/// app dir). `deploying` picks which.
async fn author_stack(state: AppState, identity: Identity, body: DeployRequest, deploying: bool) -> Response {
    if let Err(status) = require_at_least(&state, &identity, Role::Operator) {
        return status.into_response();
    }
    let app = body.stack.app().to_string();
    // App id must be a single safe path segment (it names apps/<app>/).
    if let Err(msg) = validate_rel_path(&app).and_then(|()| {
        if app.contains('/') {
            Err("stack name must be a single path segment".to_string())
        } else {
            Ok(())
        }
    }) {
        return unprocessable(format!("stack name `{app}`: {msg}"));
    }
    if deploying
        && let Err(msg) = validate_package_segment("name", &body.stack.package)
            .and_then(|()| validate_package_segment("version", &body.stack.version))
    {
        return unprocessable(msg);
    }

    let layer_dirs = match body.scope.layers() {
        Ok(d) => d,
        Err(msg) => return unprocessable(msg),
    };
    let scope_label = body.scope.label();

    // Devices scope: every id must be a live device (clear 422 over a
    // silent no-op typo). Assignment layers (all/fleet/site/type) may be
    // authored ahead of any member, so they are not existence-checked.
    if let Scope::Devices { ids } = &body.scope {
        let conn = state.db.lock().expect("db mutex poisoned");
        for id in ids {
            let live: Option<i64> = match conn
                .query_row(
                    "SELECT 1 FROM devices WHERE device_id = ?1 AND decommissioned_at IS NULL",
                    params![id],
                    |r| r.get(0),
                )
                .optional()
            {
                Ok(v) => v,
                Err(e) => return internal(e),
            };
            if live.is_none() {
                return unprocessable(format!("unknown or decommissioned device `{id}`"));
            }
        }
    }

    // Ownership + delegation gate per layer (federation §8.2/§8.4),
    // identical to PUT /api/tree/layers — deploy never bypasses it.
    for dir in &layer_dirs {
        let tree_path = format!("layers/{dir}");
        if let Err(refusal) = state.ownership.check_write(Stream::Local, &tree_path) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": refusal.to_string() })),
            )
                .into_response();
        }
        if let Err(resp) = check_not_delegated(&state, &tree_path) {
            return *resp;
        }
    }

    // Build the replacement set: each scope layer gets its apps/<app>/
    // subtree replaced (deploy: app.yaml; undeploy: nothing => removed).
    let app_yaml = if deploying {
        match serde_yaml_ng::to_string(&AppYaml {
            enabled: true,
            package: PackageRef {
                name: &body.stack.package,
                version: &body.stack.version,
            },
            profile: body.stack.profile.as_deref(),
        }) {
            Ok(s) => Some(s.into_bytes()),
            Err(e) => return internal(e),
        }
    } else {
        None
    };
    let replacements: Vec<Replacement> = layer_dirs
        .iter()
        .map(|dir| {
            let prefix = format!("layers/{dir}/apps/{app}/");
            let files = match &app_yaml {
                Some(bytes) => vec![(format!("{prefix}app.yaml"), bytes.clone())],
                None => Vec::new(),
            };
            (prefix, files)
        })
        .collect();

    let author = crate::tree::author_of(&identity);
    let verb = if deploying { "deploy" } else { "undeploy" };
    let prep = if deploying { "to" } else { "from" };
    let message = format!("{verb} {app} {prep} {scope_label}");

    let committed = {
        let mut store = state.revisions.lock().expect("revisions mutex poisoned");
        commit_replacements(&mut store, &replacements, &author, &message)
    };
    match committed {
        Ok((revision, changed)) => {
            if changed {
                // Re-render: only devices whose merged content actually
                // moved are bumped (D3), so a site deploy re-renders that
                // site's devices and no one else.
                crate::render::render_all_logged(&state);
            }
            Json(DeployResponse {
                revision,
                changed,
                app,
                scope: scope_label,
            })
            .into_response()
        }
        Err(e) => internal(e),
    }
}

/// POST /api/deploy (operator+) — deploy a stack to a scope (§11.4).
/// Authors `app.yaml` (package ref + `enabled:true` + profile) into the
/// scope's layer(s) via the tree authoring path; re-renders affected
/// devices.
#[utoipa::path(
    post,
    path = "/api/deploy",
    tag = "deploy",
    request_body = DeployRequest,
    responses(
        (status = 200, description = "Deployed (or unchanged — D14 idempotence)", body = DeployResponse),
        (status = 401, description = "Unauthenticated"),
        (status = 403, description = "Below operator role, or scope owned by another tier (federation §8.4)", body = device_api::ErrorBody),
        (status = 422, description = "Invalid stack, scope, or unknown device", body = device_api::ErrorBody),
    ),
)]
pub async fn deploy(
    State(state): State<AppState>,
    identity: Identity,
    Json(body): Json<DeployRequest>,
) -> Response {
    author_stack(state, identity, body, true).await
}

/// POST /api/undeploy (operator+) — remove a stack from a scope (§11.4:
/// "the same call removing the app from the scope"). Removes the app dir
/// from the scope's layer(s); re-renders affected devices.
#[utoipa::path(
    post,
    path = "/api/undeploy",
    tag = "deploy",
    request_body = DeployRequest,
    responses(
        (status = 200, description = "Undeployed (or unchanged — the app was not in the scope)", body = DeployResponse),
        (status = 401, description = "Unauthenticated"),
        (status = 403, description = "Below operator role, or scope owned by another tier (federation §8.4)", body = device_api::ErrorBody),
        (status = 422, description = "Invalid stack or scope", body = device_api::ErrorBody),
    ),
)]
pub async fn undeploy(
    State(state): State<AppState>,
    identity: Identity,
    Json(body): Json<DeployRequest>,
) -> Response {
    author_stack(state, identity, body, false).await
}

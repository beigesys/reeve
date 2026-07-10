# DECISIONS-MADE — judgment calls made during the autonomous build

One line each: what, where, which doc informed it.

- rusqlite gains the `session` feature at workspace level (Cargo.toml) — required by D16 changeset tier (docs/decisions/storage.md).
- crates/repo-store renamed to crates/revision-store, gix removed from workspace — direct execution of D13 (docs/decisions/delivery.md).

## A1 reeve-types
- ApplicationDescription accepts id both top-level (spec examples) and under metadata (reference sandbox artifacts); effective_id() reads either — src/margo/application.rs, informed by ApplicationDescription-001.yaml vs reference nextcloud margo.yaml
- deploymentProfiles[].type, roles[], peripheral/interface/architecture kept as String with named constants, not enums: pinned fixtures contradict the OpenAPI enums (helm.v3, 'standalone cluster' case, 'GPU', x86_64) — workload-management-api-1.0.0.yaml vs device-capabilities.md examples
- Component.properties and Parameter.value are generic serde_yaml_ng::Value: property sets are profile-type-specific and fixtures disagree on scalar types (wait: true vs "true") — wire-exact preservation
- resources.cpu modeled as untagged CpuSpec {One|Many}: OpenAPI says object, every device-capabilities.md example is an array — original shape preserved on re-serialize
- DeploymentStatusManifest.status read as an object (not []status): the attribute table's []status is contradicted by both the example manifest and the OpenAPI schema — noted as spec typo in doc comment
- Overall-state precedence 'failed > removing > installing > pending > removing > installed' has duplicate 'removing'; second occurrence read as 'removed' (only member otherwise absent) — DeploymentState::severity, deployment-status.md
- StateManifest shaped after Margo UnsignedAppStateManifest: manifestVersion + bundle{mediaType,digest,sizeBytes,url} + apps[{appId,deploymentId,secrets_version}]; bundle always serialized (null when empty) per DeploymentBundleRef's MUST-NOT-omit rule — delivery.md D13 + 08-packaging §10.2
- secrets_version spelled snake_case on the wire (exact token used normatively throughout spec/reeve/); other reeve fields camelCase matching Margo convention
- Render-bundle media type const application/vnd.reeve.render-bundle.v1+tar+gzip coined after Margo's application/vnd.margo.bundle.v1+tar+gzip — D13 names no media type
- ServerCapabilities field named serverVersion (01-framework §3.3 says 'extension list plus server version' without pinning a name)
- HealthSample/DiskSample/MemorySample inner field names (usedBytes/freeBytes/totalBytes) reeve-chosen: §7.2 pins semantics not sub-shape; #[serde(flatten)] extra map preserves extensible fields per §7.2 MUST-ignore-unknown
- Journal backfill wire types (JournalBatch/JournalRecord kinds status|health|lifecycle|gap, JournalAck.ackedSeq) defined from 05-health-journal §7.1/§7.3 prose — no example payload exists in spec
- SSE payload field types: durability-lag generation is String (generation keys are rfc3339-shaped per 07-durability §9.2), lagSeconds/lastSeq u64, rollout wave u32, secret-rotation version u64
- custom-otel-helm-app margo.yaml is parse-only in tests: it ships unsubstituted {{HELM_REPOSITORY}} mustache placeholders which YAML parses as complex mapping keys that the YAML emitter cannot re-serialize; full round-trip covered by the other three ApplicationDescription fixtures
- Spec-markdown JSON example blocks extracted at test time as fixtures (deployment-status.md, device-capabilities.md, 01-framework §3.3, 05-health-journal §7.3); ellipsis placeholder keys ("...") stripped before parsing as they are elision notation, not wire fields

## A4 margo-package
- Accepted helm.v3 as a valid deploymentProfiles[].type alongside the linkml pattern's helm|compose, since the pinned reference sandbox ships it (custom-otel-helm-app/margo.yaml; matches reeve-types profile_type consts) — anything else is an Error.
- Two-tier severity: linkml-required violations are Errors that fail Package::load_dir; divergences the pinned reference artifacts legitimately ship (parameter target naming a profile id instead of a component name, cpu architecture x86_64 outside the enum, dangling catalog resource links like license.pdf) are Warnings retained on Package.warnings — WIRE-EXACT rule says real artifacts must not be rejected, spec/margo wins on the rule itself.
- Parameter default values are checked against their linked schema's dataType only (Warning on mismatch, int widens to double); range/regex rules apply to user-provided values at configuration time — enforcing them on defaults would reject the nextcloud reference package (default port 80 vs portRange min 1024).
- Catalog resource links resolve with a lexical containment check (.. escaping the package root and absolute/URL links are never resolved to disk); missing files are Warnings.
- Skipped regex-based linkml checks (author email, url schema regexMatch) rather than adding a regex dependency; id/memory/storage patterns are hand-rolled charset checks.
- OCI refs parse oci://registry/repo[:tag][@algo:hex] with port-aware tag splitting (colon in last path segment only); PackageSource::parse also accepts dir:// per the Milestone-1 agent fetcher convention in CLAUDE.md.
- metadata.catalog + at least one organization with a name enforced as Errors per linkml required flags — all six pinned fixtures satisfy this.

## A2 desired-state
- REEVE_UUID_NAMESPACE defined as UUIDv5(NAMESPACE_DNS, "reeve.dev") = 06c32e1b-5365-5c68-80a2-6cccfa182cf8 (src/render.rs) so independent implementations can re-derive it — D2 asked for a defined namespace constant
- merged `enabled` defaults to true when unset: an app defined by any layer is desired unless explicitly switched off (D11's `enabled: true|false` read as explicit-off switch; tests/render.rs enabled_false_removes_app)
- application.yaml is the vendored package margo.yaml bytes VERBATIM, not re-emitted — wire-exact by construction (D2 + CLAUDE.md WIRE-EXACT rule)
- canonical emitter (D3) applies to ALL rendered YAML including wire-exact deployment.yaml — keys sorted lexically; semantics unchanged, determinism guaranteed
- env-targeted parameters (pointer matching ^env. case-insensitive — both ENV.X and env.x appear in pinned fixtures) trigger per-service `env_file: [env/<service>.env]` injection into every compose service; values ride in deployment.yaml parameters and the agent materializes the env files (spec/reeve/10-secrets.md §12.3 'rendered compose references them via env_file')
- v1 renders compose profiles only; helm/helm.v3 profiles are RenderError::UnsupportedProfileType (CLAUDE.md substrate rules: compose first, helm later/never)
- profile selection: app.yaml `profile:` matches DeploymentProfile.id; when absent, the sole profile or the sole compose-typed profile is used, else AmbiguousProfile error
- compose packageLocation must be package-local; URLs/absolute/escaping paths error (D11 no fetch-at-render); when absent, compose.yml then compose.yaml at package root; exactly one component per compose profile in v1
- strict tree authoring: unknown app.yaml keys, stray paths in an app dir (e.g. typo'd params.yml), and params.yaml names not declared by the ApplicationDescription are errors, never silently ignored (single-writer tree, D11/D14)
- ${REEVE_REGISTRY} substituted from device context in compose.yml, files/** (UTF-8 files only; binary files pass through verbatim), and resolved parameter values — never in the verbatim application.yaml (D3/D8)
- package.name/package.version must be YAML strings (quote numeric-looking versions) since version keys the vendored packages/<name>/<version>/ dir
- manifest.yaml field spelling: camelCase deviceId/generation/registryEndpoint/revisions.{hub optional, local} (u64 revision ids), matching reeve-types' camelCase reeve-extension convention
- uuid dep (v5 feature) pinned in the crate's own Cargo.toml, not workspace root, to avoid cross-agent Cargo.toml conflicts

## A3 revision-store
- Two streams as a CHECK-constrained TEXT column on one revisions table (not parallel tables) — single blob table shared, one monotonic id space, simpler queries; §8.2 only requires the streams be independent chains.
- Revision ids are globally monotonic across both streams (INTEGER PRIMARY KEY AUTOINCREMENT); parent chains are per-stream (parent = stream head at commit time).
- Idempotency compared against the STREAM HEAD only: re-committing content identical to an older (non-head) revision creates a new revision — matches D13's 'undo = new revision with prior content'.
- commit() takes the full tree manifest each time (root manifest model per D13), not a delta; empty manifests are allowed.
- read_at returns Ok(None) for a missing path but Err(UnknownRevision) for a missing revision id — distinguishes 'file absent' from caller bugs.
- blame(path) spans both streams, ascending id, comparing each revision's digest for the path against its own parent's; removal shows as digest=None.
- Plain rowid tables with explicit PKs (no WITHOUT ROWID) so the D16 session-extension change capture at server level can track them.
- sha2 0.10 pinned in the crate's own Cargo.toml rather than workspace.dependencies, per build-rule preference to avoid root Cargo.toml conflicts.
- Kept AUTOINCREMENT so rolled-back/killed transactions can never reuse a revision id (append-only invariant survives crashes).

## B1 manifest poll
- Default poll interval 30s (spec pins no value; 02-channel notes latency = poll interval without the channel)
- HTTP client is reqwest 0.12 pinned in-crate with default-features off + rustls-tls (no openssl); 30s request timeout so a poll can never hang
- ManifestVersion persisted as u64 bit-cast to i64 in SQLite; compared only in Rust (documented in schema) — test covers epoch 0xFFFF past bit 63
- manifestVersion 0 rejected as invalid (Margo range [1, 2^64-1], first MUST be 1) and journaled as security; first-ever manifest otherwise accepted at any valid value
- 200 response missing an ETag header falls back to sha256 digest of the body so conditional GET still works
- Error classification: unreachable (network/missing dir) journaled 'info' severity — expected offline operation; non-200/304 status or unparseable body journaled 'error'; both continue from last known state
- Bundle digest violating sha256:<hex> grammar rejects the manifest before version evaluation (it could never verify after pull)
- Acceptance is atomic: manifest_state upsert + journal row in ONE transaction; persist failure means not accepted (floor unchanged, retried next cycle)
- applied_state table created now with D5 phase CHECK (planned/applying/applied/failed/removing/removed) and record_applied/applied_apps accessors so B3 has its contract; B1 only reads
- dir:// sources never advertise capabilities (no server) => pure Margo behavior
- Capability probe runs once per startup (restart covers 'on version change'); result is informational only, convergence never depends on it (§3.2)
- Used axum (already a workspace dep) as the mock test server instead of adding httpmock

## C1 identity/auth
- Placement per task suggestion: Identity/Role/extractors + device-token machinery in device-api; human auth modes + role policy in reeve-server/src/auth/ (D1 seam shared, Law 2 kept)
- refinery is UNLINKABLE here: refinery-core 0.9.2 caps rusqlite at <=0.39 while the workspace pins rusqlite 0.40 (session feature, D16) and libsqlite3-sys `links=sqlite3` forbids two copies — shipped a minimal embedded runner keeping refinery's refinery_schema_history table shape (drop-in swap later), sha256 checksums with drift detection, one tx per migration; documented in db.rs module docs
- db::migrate() returns bool 'applied anything' so C6 can honor D16's migration-cuts-snapshot-generation law
- revision-store keeps self-initializing its own DDL on the shared single DB file (Law 2); server migrations own only server tables; two writer connections arbitrated by WAL+busy_timeout for now
- Device tokens: 'rvd_' + 64 hex (256-bit CSPRNG), stored as plain hex sha256 — sufficient preimage resistance for high-entropy random tokens; argon2 reserved for human passwords
- Identity::Anonymous carries no privilege in the type; REEVE_AUTH=none elevation to admin happens only in mode-aware AppState::effective_role (password-mode anonymous stays role-less)
- Proxy mode refuses startup unless BOTH REEVE_PROXY_USER_HEADER and REEVE_PROXY_TRUSTED_CIDR are set; missing peer address or untrusted peer => 401 fail-closed; optional REEVE_PROXY_ROLE_HEADER: absent => admin (proxy gates access), unparseable => viewer (least privilege)
- First-boot setup token is in-memory only (sha256 in AppState), logged at WARN, single-use, burned on success; crash-only: a restart while zero users exist mints a fresh one — nothing persisted
- Sessions: cookie 'reeve_session' holds raw 'rvh_' token, DB stores sha256; sliding expiry (REEVE_SESSION_TTL_SECS, default 7d) with 60s write granularity to avoid per-request writes; expired sessions purged at startup, no background reaper
- Hand-rolled ~60-line CIDR matcher (IPv4/IPv6, v4-mapped canonicalization) and manual cookie parse/set — avoided ipnet and axum-extra deps
- Cookies are HttpOnly+SameSite=Lax without Secure attribute (TLS termination is deployment-specific); noted for packaging docs
- reeve-server restructured to lib (src/lib.rs) + thin main.rs so integration tests and C2..C12 compose the same bootstrap/router
- V1 migration creates minimal devices + device_tokens tables (auth needs the FK target); C2 enrollment extends devices via a V2 migration, never recreates
- Defaults: REEVE_LISTEN 0.0.0.0:8420, REEVE_DATA_DIR ./data (DB at <data_dir>/reeve.db), REEVE_AUTH password
- login/setup return 404 outside password mode (surface does not exist); logout/me exist in all modes
- argon2id via argon2-0.5 defaults with 128-bit getrandom salt through SaltString::encode_b64 (avoids password-hash rand_core feature); dummy-hash verification on unknown usernames against timing enumeration
- No root Cargo.toml edits — all new deps version-pinned in the two crates (sha2 0.10, hex 0.4, getrandom 0.3, argon2 0.5; dev: tower 0.5, http-body-util 0.1, tempfile 3)

## B2 bundle pull
- Atomic dir swap implemented as content-addressed dirs + symlink flip: bundles unpack to work/, validate, fsync, rename to data_dir/bundles/<hex> (presence there ALWAYS means complete+verified), then one rename(2) of a pre-made relative symlink data_dir/bundle -> bundles/<hex>; kill -9 leaves either old or new target, never neither (rename over a non-empty dir is not atomic on Linux; symlink rename is)
- Recovery direction is roll-FORWARD: the swap is the commitment point; if kill -9 lands between swap and DB record, startup recovery records the disk digest (journal event bundle-rolled-forward); if the recorded bundle vanished from disk (external interference), the record is cleared (notable journal event bundle-state-cleared)
- bundle.url interpretation: for http(s) sources it is the OCI repository base (absolute URL, or server-relative /v2/<name> joined to the configured server origin; a full .../manifests/<digest> URL is trimmed to its repo base); for dir:// sources it is an OCI layout directory path resolved relative to the manifest source dir (blobs/sha256/<hex> read directly) — oras/skopeo layout output is directly consumable
- bundle.digest names the OCI image manifest (not the layer); the manifest bytes are verified against it, then exactly ONE layer with mediaType application/vnd.reeve.render-bundle.v1+tar+gzip is required and its blob verified against the layer digest; zero or multiple render layers fail closed
- Unpack is fail-closed: only Regular and Directory tar entries accepted (symlinks/devices/hardlinks rejected, not skipped); any path component that is absolute or .. rejects the whole bundle; file bytes fsynced at write, dirs fsynced bottom-up before the publishing rename
- Bundle bytes are buffered in memory during fetch (render bundles are config-scale; digest verification needs the whole payload anyway); HTTP client timeout 120s
- manifest bundle:null (zero apps) is a no-op for B2 — the current bundle link is left in place; removal convergence belongs to B3 per D5
- sizeBytes never enforced (advisory per reeve-types BundleRef doc; digest is the sole integrity check)
- main.rs runs BundleStore::sync on NotModified (304) as well as Accepted, because 304 does not imply the bundle is in place — an accept whose pull failed or crashed must retry; sync short-circuits (no fetch, no journal) when the recorded+linked digest already matches
- Unreachable pull failures journal at info severity (Law 5: offline is expected operation), all other pull failures at error; GC of old bundles/ dirs is best-effort (failure costs disk, never correctness)

## C2 enrollment
- Enroll wire types live in reeve_types::reeve::enroll (additive) so device-api (serves) and reeve-agent (calls) share one shape; field names snake_case exactly as written in D4 step 1, response {device_id, device_token, resumed}
- Route placement: POST /api/reeve/v1/enroll in crates/device-api behind an EnrollmentService trait (Law 2: no SQLite in device-api); join-token MANAGEMENT (POST/GET /api/join-tokens, DELETE /api/join-tokens/{token_hash}) in reeve-server behind human auth with role >= operator enforced in-handler
- Idempotent re-run (same unexpired token + same hostname) returns the SAME device with a FRESH token — returning the same token is impossible since only its hash is stored; all prior device tokens are revoked in the same tx (D1: one live credential per device); the re-run consumes NO additional use (matched via devices.enrolled_with = join token hash)
- Atomicity vs D4's 'one SQLite tx': all server-table writes (token validate + use count, device row, token revoke+issue) are ONE IMMEDIATE tx; the revision-store device-layer commit is sequenced AFTER on the store's own connection to the same DB file (Law 2 forbids reaching into its tables). Crash between the two leaves an enrolled device with an absent layer dir — semantically identical to an empty layer (D3: absence = inherit) and repaired by the idempotent retry
- Initial desired state = empty device layer marker layers/30-device.<device_id>/.keep committed to the LOCAL stream as a whole-tree snapshot carrying the head forward; author 'system:enroll'; idempotent (present at head => no new revision)
- Stale flagging (D4 wiped-box): plain-token enrollment sets stale=1 on every other device with the same hostname; idempotent re-run and re-enroll clear stale=0
- device_id = 'dev-' + 16 lowercase hex (64 bits CSPRNG); PK collision fails the insert loudly rather than merging identities
- Join tokens: 'rvj_' + 64 hex, sha256-hashed at rest, defaults 24h TTL / 1 use; DELETE revokes (sets revoked_at) rather than deleting rows (audit trail); enroll 401 is deliberately indistinguishable across unknown/expired/exhausted/revoked
- Re-enroll token creation validates the target device exists (404 otherwise); binding enforced by FK with ON DELETE CASCADE
- Agent: hostname detected from /proc/sys/kernel/hostname, /etc/hostname, then $HOSTNAME (no new dependency); reqwest 'json' feature added to reeve-agent; enroll subcommand parsed by hand (two required flags do not justify a CLI framework); server URL trailing slash trimmed before persisting

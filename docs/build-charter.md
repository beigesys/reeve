Full implementation. The specs (spec/reeve/), decisions
(docs/decisions/), and CLAUDE.md are complete, adjudicated LAW.
Build EVERYTHING that is decided — every crate, every extension,
the UI, packaging — driving every thread, every surface, every
tree to completion. The operator is AWAY. There are no review
checkpoints and nobody to ask. You run until every track is either
COMPLETE or truly, massively blocked.

THE BLOCKER BAR IS EXTREMELY HIGH. A blocker is something that
makes correct code IMPOSSIBLE to write, not uncomfortable:
two normative MUSTs that directly contradict so both cannot be
satisfied, or a wire format the code must emit that no document
defines at all. That is the whole list. Everything below the bar
is a JUDGMENT CALL: make the call, implement it, record one line
in DECISIONS-MADE.md (what, where, which doc informed it), and
keep moving. Ambiguity is not a blocker — pick the reading most
consistent with the Laws and the surrounding spec. A missing
detail is not a blocker — choose the boring option. A test you
can't make pass is not a blocker — it is a bug; fix it. A
dependency question is not a blocker — build the path that needs
no new dependency, note it. If you catch yourself writing a
BLOCKERS.md entry, first ask: "could a competent engineer with
these documents ship SOMETHING defensible here?" If yes, you are
that engineer — ship it and log the call.

Even a true blocker stops ONLY its own thread: record it in
BLOCKERS.md with exact doc citations AND implement the best
placeholder that keeps every dependent thread building (a stub
honoring the agreed interface), then continue everywhere else.
The run ends when all tracks are complete or stubbed-past-a-
logged-blocker — never earlier, never because of accumulated
small doubts.

Fences that are NOT blockers, just boundaries:
- The NOT-decided list (settings envelope, coordinated secret
  rotation, cohort selector UX, operator taxonomies, multi-class
  devices, RBAC beyond three roles, mTLS/9421) stays UNBUILT —
  stub the seam if one is spec'd, otherwise nothing. Not a
  blocker; simply out of scope.
- DECISIONS items are never relitigated. If one seems wrong,
  build to it anyway and note the concern in DECISIONS-MADE.md;
  only a literal self-contradiction reaches BLOCKERS.md.

BUILD GRAPH — order within a track matters, tracks parallelize:

Track A — foundations
  A1 reeve-types: all Margo wire types + reeve extension types
     (manifest with (epoch,counter), capability advertisement,
     health payload, SSE event types). Round-trip tests against the
     ACTUAL fixtures in spec/margo/ + reference/ — never
     hand-written approximations. Spec citations in doc comments.
  A2 desired-state: pure render per tree-render.md. Table tests
     first; full fixture set: empty tree, single device, override
     precedence, list-replace, null-delete, whole-file replace,
     3-layer inheritance, class layer, enabled:false, pinned device
     under rollout, byte-identical re-render, deterministic
     deploymentId, ${REEVE_REGISTRY} from device context.
  A3 revision-store: content-addressed blobs + append-only
     revisions (SQLite). Two streams per tier (upstream/local,
     federation §8.2). Diff/blame/read-at-revision as queries.
     Kill -9 mid-commit chaos test.
  A4 margo-package: parse/validate packages from vendored dirs.

Track B — reeve-agent (after A1)
  B1 manifest poll: conditional GET, ETag, epoch/counter rules
     (epoch bump = notable log, counter regression = security
     event), offline = no-op continue-from-applied.
  B2 bundle pull by digest from /v2, verify, unpack temp, atomic
     swap.
  B3 compose provider: journal phases, down-before-delete via
     retained applied/ copy, per-service env files, resumable from
     any kill point. Status reports wire-exact.
  B4 secrets fetch: resolve endpoint, per-service env
     materialization, secrets_version-driven minimal re-up.
  B5 persistent channel client: outbound ws, reconnect/backoff,
     nudge handling (optimization never correctness), sub-channel
     mux.
  B6 terminal sub-channel: PTY via portable-pty, enabled only via
     desired state.
  B7 health sampler + local status journal + backfill-on-reconnect
     with original timestamps.
  B8 self-install (idempotent), self-update via the agent-as-
     artifact package (A/B on binary path, failed update leaves old
     binary running).

Track C — reeve-server (after A1-A4)
  C1 identity/auth: Identity extractor seam; password mode
     (argon2id, sessions, first-boot setup token), proxy mode
     (trusted-CIDR refusal), none mode (loud warning). Device
     tokens hashed. Roles admin/operator/viewer.
  C2 enrollment (D4): join tokens, atomic device+repo creation,
     idempotent re-run, re-enroll tokens.
  C3 authoring API (D14): idempotent layer puts -> revisions;
     ownership enforcement (structural, per-tier ownership set).
  C4 render pipeline: revision -> affected-device bundles ->
     native read-only OCI serve; manifest endpoint.
  C5 status ingest incl. late backfill idempotency; presence.
  C6 durability: snapshot tier (VACUUM INTO, AEAD under external
     keyfile, atomic upload, retention) + changeset tier (session
     extension, sequenced encrypted uploads, migration-cuts-
     generation) + verify-restore (whole chain + epoch marker
     assertion) + crash-only restore-at-bootstrap + epoch fencing
     (increment-then-serve).
  C7 secrets vault: AEAD table, scoping down the chain, resolve
     endpoint, write-only UI semantics, rotation -> secrets_version
     propagation.
  C8 device channels: ws server, presence-as-fact, nudges, terminal
     byte bridge (relay only, full audit rows incl. username,
     optional session recording), SSE endpoint with the §6.3 event
     table incl. durability-lag.
  C9 rollouts: cohorts (explicit lists, tree selections, labels-as-
     grouping), waves, health gates, auto-pause, convergence target
     = device's own render, pinned/unaffected surfacing, rollback =
     new rollout.
  C10 federation: upstream sync client (revisions conditional-GET +
     content-addressed blobs), scoped secrets sync with per-tier
     re-encryption, status forwarding with backfill, air-gap
     export/import (signed OCI layout archives, secrets to gateway
     pubkey), REEVE_UPSTREAM tier selection.
  C11 zot /v2 reverse proxy (device-token termination, backend
     credential injection) behind config.
  C12 packaging: single static musl binaries both arches, embedded
     UI dist + migrations + openapi + spec/reeve/ (--spec [name]),
     reeve-server init (compose/units/zot config emission,
     keyfile + separate-backup warning), /install endpoint behind
     embedded-agents feature.

Track D — UI (after C1-C5 exist enough to generate the API)
  D1 utoipa on every route -> openapi.json -> orval generation
     (just gen-api; CI drift check). No hand-written API types.
  D2 Full UI per ui.md: TanStack Router/Query/Table, shadcn.
     Views: device fleet (presence, health, labels), device detail
     (status, journal, render provenance, terminal via xterm.js),
     tree/layer editor with diff + revert (revision history, blame),
     app catalog/packages, rollout dashboard (waves, gates,
     pinned/unaffected), secrets (write-only, metadata, rotate),
     enrollment (join tokens), settings/ops (verify-restore status,
     durability lag, epoch, federation state). SSE -> Query
     invalidation via generated key factories; polling fallback.
  D3 SPA fallback serving, vite proxy dev mode.

Track E — verification (continuous, plus final)
  E1 e2e suite: the M1-harness loop (author/converge/pin/kill -9
     server mid-render/agent mid-apply/restore-with-epoch-bump),
     PLUS: rotation bounces only consuming services; rollout wave
     halts on failed gate; terminal session end-to-end with audit
     row; federation e2e (root + gateway + device: authored at
     root, synced, rendered at gateway, converged at device;
     gateway serves through simulated WAN outage; status backfills
     on reconnect); air-gap export/import round-trip; changeset
     restore to a sequence point.
  E2 conformance job: full e2e core loop with ALL extensions
     compiled out/disabled — required CI job.
  E3 just standalone, cargo test --workspace, clippy clean, ui
     build + drift check.

CODE BOUNDARY — standard vs extension (the compiled version of the
reference implementation's `non-standard/` separation). BE CLEAR ON
SCOPE: "extension" is an architectural boundary, NOT an optionality
signal. Every extension in the build graph is a MANDATORY
deliverable of this run — the channel, terminal, SSE, health,
federation, rollouts, secrets, and changeset durability are the
product; the Margo-shaped core alone is NOT a completed run. The
--no-default-features conformance build is a TEST CONFIGURATION
proving additivity, not a shipping target and not a scope you may
stop at. A run that ends with any ext-* feature unimplemented and
no BLOCKERS.md entry justifying it is an incomplete run.
Mechanics: every reeve
extension is gated behind a cargo feature, one per extension, named
ext-*: ext-channel, ext-terminal, ext-sse, ext-health,
ext-federation, ext-rollouts, ext-secrets, ext-durability-changeset,
plus embedded-agents (already named). All default-ON. Margo-shaped
code — wire types, manifest poll/serve, enrollment, render, bundles,
snapshot durability, compose provider, auth — is unconditional core,
never feature-gated. Rules: extension code lives in clearly named
modules (src/ext/<name>.rs or an ext- prefixed crate if it earns
one) so the feature gate covers whole modules, not scattered cfg
lines; core MUST NOT depend on any ext-* item (compiler-enforced by
the conformance build); an extension MAY depend on another (terminal
on channel) and the feature graph declares it. The conformance job
(E2) is exactly: build server+agent with --no-default-features
--features <core set>, run the core-loop e2e. Capability
advertisement (framework spec) is derived from compiled-in features
— an agent literally cannot advertise what it doesn't contain.
Document the feature list in 01-framework's conformance section and
in CLAUDE.md's routing map.

GIT DISCIPLINE — commit and push continuously. You are unattended;
the remote is the flight recorder and the only thing that survives
a dead session. Commit at every coherent unit (a crate compiling
with tests, a build-graph item done, an e2e passing — roughly every
30-60 min of work, never less often), conventional-commit style
with the build-graph id (e.g. "feat(agent): B3 compose provider
journal phases"). Push after EVERY commit — an unpushed commit
does not exist. Commit working states; if you must checkpoint
mid-surgery, say WIP in the subject and fix forward. Never
rebase/force-push/amend anything already pushed; never commit
secrets, keyfiles, or *.db. DECISIONS-MADE.md and BLOCKERS.md are
committed as they grow, in the same commit as the work that
prompted the entry. Everything on main — no branch ceremony for an
autonomous run; the linear history IS the run log. If the push
remote is missing or rejects: verify with `git remote -v`, log it
in BLOCKERS.md, and keep committing locally — never let a remote
problem stop the build.

Standing rules, every line: crash-only (a shutdown handler or a
recovery path distinct from startup = stop and reread Law 3); every
file write temp+fsync+rename; every SQLite write transactional; PKs
on every table; substrate-blind services; secrets plaintext only in
RAM/TLS/device env files; no unrequested features; "works" and
"done" differ — done means the chaos checks pass.

FINAL DELIVERABLE: everything above building and green, plus
REPORT.md — per track: what's built, test counts, e2e output;
DECISIONS-MADE.md (every judgment call, one line each — this is
the review surface for the absent operator, so completeness here
buys the autonomy); BLOCKERS.md contents (expected: empty); and
the exact commands to run the full stack (server + two agents) on
this machine. If you finish everything with budget remaining, do
NOT invent features — spend the remainder on Track E: more chaos
tests, more e2e permutations, property tests on desired-state,
fuzzing the manifest poll and package parsers.

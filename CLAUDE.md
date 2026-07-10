# reeve — fleet desired-state manager (Margo-inspired)

reeve (server) compiles desired state; reeve-agent (per box) converges on it.
The server compiles a layered deployment tree into per-device git repos.
reeve-agent pulls its repo, converges the box, reports status.

## The Five Laws
1. **Spec-grounded.** Implement against the pinned Margo spec in `spec/`
   (PR2 snapshot). Never from memory of Margo. Where a type mirrors the
   spec, cite the spec file/section in a doc comment. We adopt Margo's
   *decisions* (app model, per-device desired-state repo, pull agent,
   WFM/DFM split); we are not chasing conformance.
2. **Every crate stands alone.** Each crate compiles and passes tests by
   itself (`cargo build -p <crate>`). No crate reaches into another's
   internals. Smallest useful unit per crate.
3. **Crash-only.** No shutdown ceremony anywhere. Startup IS recovery.
   `kill -9` mid-operation must leave resumable state. Writes are atomic
   (temp+rename) or transactional (SQLite). Idempotent startup, always.
4. **State lives in engines with someone else's test suite.** Server
   runtime state: SQLite (WAL). Desired state: bare git repos on disk.
   Nothing load-bearing lives only in RAM. Config in files; settings in
   the DB. Never commit values, only shape (.env rule).
5. **Offline-first agent.** reeve-agent assumes it is offline more than online.
   Every network call has a "couldn't reach — continue from last known
   state" path. Polling, outbound-only, NAT-native. This is the gap the
   Margo spec defers; we do it properly.

## Substrate rules
- Services are substrate-blind: no orchestrator APIs, no cluster
  assumptions. reeve-agent applies workloads through the `Provider` trait —
  compose first, systemd units second, helm later/never.
- Operational contract baked in from line one: SIGTERM-clean, /healthz,
  structured logs to stdout, config via env/files, externalized state.

## Layout
- crates/reeve-types    — Margo-shaped types (ApplicationDescription,
                          deployment profiles, status). serde only.
- crates/margo-package  — parse/validate app packages (dir or OCI ref).
- crates/desired-state  — THE crate: overlay tree -> rendered per-device
                          state. Pure functions. Zero I/O. Table-tested.
- crates/repo-store     — bare repos on disk via gix. commit/read/render
                          plumbing. No shelling out to git.
- crates/device-api     — axum routes: enroll, status ingest.
- crates/reeve-agent   — agent binary: fetch -> diff -> apply -> report.
- crates/reeve          — server binary: ties it together + embedded UI.

## Build order
reeve-types -> desired-state -> repo-store -> reeve-agent(compose provider)
-> device-api -> reeve -> UI. Milestone 1: full loop against a local
bare repo with `git daemon`, no server at all.

## Verification
- `cargo test --workspace` and per-crate `cargo build -p <crate>`.
- desired-state: table tests (tree in, files out) are the spec.
- Chaos check before calling anything Done: kill -9 the process
  mid-operation, restart, assert convergence.

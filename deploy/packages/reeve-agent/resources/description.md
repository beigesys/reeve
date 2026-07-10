# reeve-agent

The reeve per-device agent as a Margo-shaped application package
(spec/reeve/08-packaging.md §10.5): agent updates are authored,
staged, and converged through the SAME desired-state tree the agent
exists to apply — no side-band updater exists.

## How an update applies

1. This package is authored into the tree at some tier and staged
   like any workload (spec/reeve/09-rollouts.md Section 11).
2. The render pipeline places `agent-update.yaml` (see
   `resources/agent-update.yaml`) into the device's app dir, naming
   the new binary's OCI blob and digest.
3. The agent prefetches the binary by digest, stages it BESIDE the
   running one (`reeve-agent-<version>`), atomically swaps the
   `current` symlink, and exits for its unit to re-exec the new
   binary. A/B: the `previous` symlink retains the old binary.
4. A new binary that cannot survive its first health window fails
   the unit; the `OnFailure=` companion runs `previous rollback` —
   through the retained binary — flipping `current` back and holding
   the bad version until desired state names a different one.
5. The agent reports its version in status; that is how a staged
   rollout's health gates observe success.

`kill -9` at any point leaves old-running or new-running — never
neither (CLAUDE.md Law 3).

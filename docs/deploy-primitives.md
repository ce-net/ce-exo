# Deploy/exec primitives in the CE workspace — a map (and what ce-exo depends on)

This exists because the workspace has **many overlapping ways to run something on another machine**,
and it's confusing — even to the owner. This is an observed survey (not authoritative) plus the one
decision ce-exo makes so it doesn't add to the pile.

## What's out there (observed)

| App | What it does | Mechanism | Sandboxed? |
|---|---|---|---|
| `rdev` | remote exec + file sync + **host process spawn/run** | mesh topics `rdev/exec`, `rdev/spawn`, `rdev/run/*`, `rdev/sync` | `exec`=container (network off); `run`/`spawn`=**host process** |
| `replicator` | self-replicating fleet bootstrap (tree, attenuating caps) | composes `rdev/sync` + `rdev/spawn` | host (via rdev) |
| `ce-gke` | declarative orchestrator ("GKE for the mesh"): Deployments, replicas, reconcile, rollout | `mesh-deploy` / `mesh-kill` (node RPC) | **container, `network_mode=none`** |
| `ce-fleet` | installer + fleet enrollment + admin console | reuses `replicator` + `ce-cap` org root | host (via replicator/rdev) |
| `ce-link` | add a device to your fleet in one command | `ce grant` + hub handshake + relay | onboarding only (no exec) |
| `ce-ci` | sharded test/CI runner | sandboxed container per shard | container |

The key split: **container deploys** (`ce-gke`, `mesh-deploy`) run with `network_mode=none` — great
for untrusted compute, useless for a long-lived service that needs the network/GPU. **Host-process
launch** (`rdev/run`, `replicator`) runs real code on the host with full access.

A long-lived **exo worker needs host launch** (network + GPU + reach its node), so ce-exo needs the
host-process path, not the container path.

## What ce-exo depends on

ce-exo deploys a worker by sending one mesh request — `<ns>/run/start {caps, cmd, cwd}`, gated by the
`spawn` capability — to the target's host-exec service. It does **not** hardcode an app:

- `ns` is a single constant, [`orchestrate::host_launch_ns`](../crates/ce-exo-router/src/orchestrate.rs),
  **default `rdev`** (the installed implementation; ce-exo's `tests/deploy_e2e.sh` proves it works,
  6/6, against a real target).
- Override with `CE_EXO_LAUNCH_NS=<name>` to repoint at whatever you make canonical — no code change.

So ce-exo rides today's working mechanism without betting on a name. When the dust settles, set
`CE_EXO_LAUNCH_NS` (or change the one constant) and ce-exo follows.

## Recommendation (to reduce the confusion)

Pick **one** canonical host-exec primitive and make the rest thin clients of it or retire them:

- Keep the container orchestrator (`ce-gke`) for sandboxed marketplace compute — it's a different job.
- Choose one **host-launch** owner (today that's `rdev`'s `run/spawn` protocol; `replicator` already
  builds on it). Give it a stable name + topic namespace and point `ce-fleet`, `ce-exo`, etc. at it.
- `ce-link` stays as onboarding (issue the grant), not exec.

ce-exo is already structured to follow that decision with a one-line repoint. Tell me the name and
I'll set it.

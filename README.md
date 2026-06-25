# ce-exo

Run LLMs distributed across your CE mesh. ce-exo wraps a **real inference engine**
([exo](https://github.com/exo-explore/exo) by default; also llama.cpp / Ollama / vLLM) as a CE-native
app and adds the part those engines lack: **being a network across NAT.** It exposes the cluster
through an OpenAI- and Ollama-compatible API, a CLI, a lightweight SDK, and a built-in web UI.

ce-exo does **not** reimplement distributed inference — exo already splits a model across machines
(pipeline/tensor parallel, MLX on Apple Silicon, tinygrad elsewhere). ce-exo's job is the CE layer:

- **Stitch the cluster across NAT.** exo nodes must reach each other; across your NAT'd machines they
  can't. ce-exo opens CE **tunnels** (the mesh's raw-stream primitive, with relay/NAT traversal) so
  each machine reaches its peers at `127.0.0.1:<port>` and hands exo that as manual discovery. Laptop,
  desktop, and cloud relay form **one exo ring over the mesh**.
- **Authorize with capabilities.** Joining the cluster and calling inference are gated by `ce-cap`
  chains (`exo:host`, `exo:infer`, `exo:shard`, `exo:admin`) — no device lists, no shared secrets.
- **One public surface.** The router is the single HTTP domain; everything else is mesh-internal.
- **(Optional) bill it.** Per-request payment over CE channels — planned.

It is an app **on CE primitives**: it talks only to a local CE node via `ce-rs` and adds no node
endpoints.

## Crates

| Crate | Kind | Role |
|---|---|---|
| `ce-exo-core` | lib | Hardware probe, model registry, the auto-mode replica/pipeline **planner**, wire protocol, capability abilities. Pure + unit-tested. |
| `ce-exo-worker` | bin `ce-exo-worker` | Per-node agent: probe → advertise → **wrap + supervise a real engine** (exo / llama.cpp) → serve capability-gated requests over the mesh. |
| `ce-exo-router` | bin `ce-exo-router` + lib | OpenAI/Ollama HTTP front door + web UI; discovers workers, picks least-loaded, dispatches; **`cluster` module opens CE tunnels to stitch exo across NAT**. |
| `ce-exo-sdk` | lib | Lightweight client (`ExoClient`) — chat/stream/models against the router; optional direct-to-worker `mesh` feature. |
| `ce-exo-cli` | bin `ce-exo` | The umbrella tool: `serve`, `cluster`, `router`, `up`, `chat`, `fleet`, `models`, `status`. |

## How a multi-machine cluster forms

```
 Apple Silicon (NAT)        Linux + NVIDIA (NAT)          cloud relay
  exo (native, MLX)          exo (container, --gpus)       exo
      ▲   │                      ▲   │                      ▲
      │   └── ce-exo-worker      │   └── ce-exo-worker      └── ce-exo-worker
      │         │                │         │
      └─────────┴──── CE tunnels (libp2p, NAT traversal, ce-cap gated) ──────┘
                          exo's peers = 127.0.0.1:<tunnel ports>
                                         │
                                   ce-exo-router  ──HTTP──▶ clients / UI / SDK
                                  (OpenAI /v1, Ollama /api)
```

On **Apple Silicon**, exo runs **natively** (Docker has no Metal GPU passthrough). On **Linux/NVIDIA**
it can run in a container (`--gpus all`). ce-exo doesn't decide that — you pass the launch command.

## Quick start (single machine, no GPU, mock engine)

```bash
# A CE node must be running locally (ce start).
cargo build --release

ce-exo serve --backend mock --open &          # contribute this machine (deterministic mock engine)
ce-exo router --models models.toml &          # public front door + web UI on :8088
ce-exo fleet                                  # who's online
ce-exo chat mock-tiny "hello"                 # talk to it
open http://127.0.0.1:8088                    # the web UI
```

## Real exo, the seamless way (deploy from ONE machine)

Distribution is central. You do **not** run anything on the other machines.

**One-time per host (consent — unavoidable, you can't run code on someone's machine without it):** the
machine runs its CE node + `rdev serve` and grants you a capability with the `spawn` ability:

```bash
# on the host, once:  ce start   &&   rdev serve   &&   grant you spawn(+tunnel)
```

**After that, deploy everywhere from your machine with one command:**

```bash
ce-exo deploy llama-3.1-8b \
  --engine-cmd 'exo --chatgpt-api-port 52415' \   # how each host launches exo
  --grant <spawn-cap-token>                        # what the hosts granted you
# auto-picks nodes from the atlas; or name them: --node <node_a> --node <node_b>

ce-exo router --models models.toml                 # one public API + UI for the whole cluster
```

`deploy` launches the worker on each target **host** over the mesh via rdev `run` (a detached host
job with full node/GPU access — *not* a sandboxed `network=none` cell, which couldn't reach its peers
or the GPU). No SSH loop, no per-host setup beyond the one-time grant.

> The low-level pieces still exist for advanced/manual use — `ce-exo serve` (run a worker on this
> machine) and `ce-exo cluster` (open the CE tunnels by hand) — but the product is `ce-exo deploy`.
> If you find yourself running commands on every machine, you're using the plumbing, not the tool.
>
> `deploy` launches each worker through CE's host-exec primitive (a `<ns>/run/start` mesh request,
> `spawn`-capability gated). ce-exo doesn't hardcode which app provides it: the namespace defaults to
> `rdev` (installed + E2E-proven) and is repointable with `CE_EXO_LAUNCH_NS`. The workspace has
> several overlapping deploy/exec apps — see [docs/deploy-primitives.md](docs/deploy-primitives.md)
> for the map and how ce-exo stays decoupled. Hosts need `ce-exo` + the engine installed today;
> binary push is roadmap P1.

## API & SDK

Any OpenAI or Ollama client works against the router (`/v1/chat/completions`, `/v1/models`,
`/api/chat`, `/api/generate`, `/api/tags`). Native SDK:

```rust
use ce_exo_sdk::{ExoClient, msg};
let exo = ExoClient::new("http://127.0.0.1:8088");
let reply = exo.chat("llama-3.1-8b", vec![msg::user("Explain CE in one line")]).await?;
```

## Security

Capability-gated via `ce-cap` (`exo:infer` / `exo:host` / `exo:shard` / `exo:admin`, plus the
`exo:model:<prefix>` attenuation). `--open` disables enforcement for single-user dev only. Tunnels
require the `tunnel` ability on each member.

## Testing

```bash
cargo test --workspace        # unit tests (core planner, caps, backends, cluster, orchestrate)
cargo build --release         # build the binaries
tests/e2e.sh                  # live end-to-end: 2 ephemeral CE nodes + worker + router, real
                              # cross-node mesh dispatch through the OpenAI/Ollama API (mock engine)
```

`tests/e2e.sh` is a genuine end-to-end: it starts two isolated CE nodes, runs a worker on one and the
router on the other, and drives real chat/stream/error requests through the mesh. It needs the `ce`
binary; no GPU or exo required (mock backend). 12/12 checks pass against live nodes.

```bash
tests/deploy_e2e.sh           # proves the SEAMLESS DEPLOY path end to end
```

`tests/deploy_e2e.sh` proves the deploy model for real: a deployer node issues `ce-exo deploy`, which
launches a worker on a *different* target host over the mesh via rdev `run` (gated by a `spawn`
capability the target self-issued), and the router then routes inference to that deployed worker.
6/6 checks pass — no manual per-machine setup beyond the one-time consent (`rdev serve` + grant).

> Discovery note (surfaced by the E2E): a directed mesh request to your **own** node id fails (a node
> can't dial itself), so the router and a worker must be on **different** nodes — which is the whole
> point of distribution. Pin workers explicitly with `ce-exo router --worker <node_id>` for static
> fleets; otherwise the router discovers them via the DHT.

## Why this matters (the bigger picture)

ce-exo is the first example of a pattern that is itself the reason to use CE: **wrap a legacy backend
once, and get global, secure-by-default, pay-per-use distribution** — deployed and managed from one
place, callable type-safely from anywhere, configurable from a public UI that never holds your keys.
CE supplies the mesh, NAT traversal, capability auth, on-device key storage, and credit billing; the
wrapped engine just does its job.

- [docs/why-ce.md](docs/why-ce.md) — the motivation and developer experience (read this first).
- [docs/wrapping.md](docs/wrapping.md) — the wrap-a-legacy-backend thesis + the reusable template.
- [docs/roadmap.md](docs/roadmap.md) — seamless deploy, public-frontend secure config, typed
  multi-language SDKs, deployment management/monitoring in the graph, rent-anywhere pay-per-use.

## Docs

- [docs/architecture.md](docs/architecture.md) — components, cluster formation, trust, what's wired vs planned.
- [docs/protocol.md](docs/protocol.md) — the mesh wire protocol (topics + message types).

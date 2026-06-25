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

Distribution is central. You do **not** run anything on the other machines. Once a node is on your
mesh and has granted you a capability (a one-time consent), you bring a model up across the whole
fleet with one command from one place — each deploy is directed, capability-gated, and billed in
credits:

```bash
ce-exo deploy llama-3.1-70b            # auto-picks nodes from the atlas, deploys worker+exo to each
# or name targets: ce-exo deploy llama-3.1-70b --node <node_a> --node <node_b>
ce-exo router --models models.toml     # one public API + UI for the whole cluster
```

That's the point of CE: no SSH loop, no per-host setup. (The orchestrator is wired against CE's
`mesh-deploy` primitive; the worker container image is the remaining piece — see
[docs/roadmap.md](docs/roadmap.md) Phase 1.)

> The low-level pieces still exist for advanced/manual use — `ce-exo serve` (run a worker locally) and
> `ce-exo cluster` (open the CE tunnels by hand) — but the product is `ce-exo deploy`. If you find
> yourself running commands on every machine, you're using the plumbing, not the tool.

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

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

## Run real exo

```bash
# On each machine: wrap (and optionally launch) a local exo instance.
ce-exo serve --backend exo \
  --engine-cmd 'exo --chatgpt-api-port 52415' \   # native on Apple Silicon
  --engine-url http://127.0.0.1:52415 --open

# Stitch the machines into one exo ring over the mesh (run on each machine, listing all members):
ce-exo cluster \
  --member <node_a_hex>:<exo_peer_port> \
  --member <node_b_hex>:<exo_peer_port>
# -> opens CE tunnels and prints the 127.0.0.1:<port> peers to give exo as manual discovery.

# One public API + UI for the whole cluster:
ce-exo router --models models.toml
```

On Linux/NVIDIA, swap the engine command for a container, e.g.
`--engine-cmd 'docker run --rm --gpus all --network host <exo-image>'`.

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

## Docs

- [docs/architecture.md](docs/architecture.md) — components, cluster formation, trust, what's wired vs planned.
- [docs/protocol.md](docs/protocol.md) — the mesh wire protocol (topics + message types).

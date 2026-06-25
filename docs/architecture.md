# ce-exo architecture

ce-exo runs LLMs across a fleet of CE nodes by **wrapping a real inference engine** and supplying the
CE layer around it. It is an app on CE primitives — it talks only to a local CE node's HTTP API via
`ce-rs`, adds **no node endpoints**, routes coordination over the mesh, and authorizes with `ce-cap`.
A cluster exposes exactly one public HTTP domain (the router); everything else is mesh-internal.

The deliberate split: **the engine does inference; CE does the network.** exo (or llama.cpp) already
knows how to split a model across machines. What it can't do is connect machines that sit behind
different NATs, or gate who may join, or bill for it. That's ce-exo.

```
 Apple Silicon (NAT)        Linux + NVIDIA (NAT)          cloud relay
  exo (native, MLX)          exo (container --gpus)        exo
      ▲   │                      ▲   │                      ▲
  ce-exo-worker              ce-exo-worker              ce-exo-worker
      │   └──────────── CE tunnels (libp2p, NAT traversal, ce-cap) ───────────┘
      │                 exo manual-discovery peers = 127.0.0.1:<tunnel ports>
      ▼
 ce-exo-router  ──HTTP (OpenAI /v1, Ollama /api, web UI /)──▶  clients / SDK / browser
```

## Components

### ce-exo-core (pure)
- **probe** — cores, RAM, GPU/VRAM (best-effort, env-overridable), and a derived weight budget.
- **registry** — `models.toml`: per-model size, layer count, context.
- **plan** — the auto replica-vs-pipeline planner (used by `ce-exo up` to advise placement).
- **proto** — JSON wire types + mesh topics.
- **caps** — `exo:*` abilities and the `exo:model:<prefix>` attenuation.

### ce-exo-worker — the node agent
A per-node mesh service (`ce-rs::serve`) that:
1. **Wraps a real engine** via [`Backend`]: `mock` (deterministic, offline) or `EngineBackend` — any
   OpenAI-compatible server: **exo** (default, port 52415), llama.cpp, Ollama, vLLM.
2. **Optionally launches + supervises** that engine (`--engine-cmd '<shell>'`): spawns it, waits for
   its `/v1/models` to come up, and kills it on exit. The command is a plain string so it works
   across engine versions and platforms (native exo on Mac, containerized exo on Linux, llama-server).
3. **Probes + advertises** capacity and each resident model on the DHT (`exo-host`, `exo-model:<id>`).
4. **Serves capability-gated inference** over the mesh (`ce-cap`, unless `--open`), forwarding to the
   wrapped engine.

### ce-exo-router — the public surface
OpenAI/Ollama HTTP front door plus the built-in web UI (served at `/`). Discovers workers over the
mesh, sorts least-loaded (GPU/budget break ties), dispatches an `InferRequest`, retries on failure.
Its **`cluster` module** opens CE tunnels (`POST /tunnel` on the local node) to wire an exo ring
across NAT and emit each machine's manual-discovery peer list.

### ce-exo-sdk
Thin `reqwest` client over the router (OpenAI dialect): `chat`, `chat_stream`, `models`. Optional
`mesh` feature for direct-to-worker dispatch.

### ce-exo-cli (`ce-exo`)
`serve` (run the node agent), `cluster` (stitch exo across machines), `router`, `up` (advise
placement), `chat`, `fleet`, `models`, `status`.

## Cluster formation (the CE value-add)

`ce-exo cluster --member <node>:<exo_peer_port> ...`, run on each machine:

1. Resolve this machine's node id (or `--self`).
2. For every *other* member, allocate a local port and open a CE tunnel
   `127.0.0.1:<local> → <member node>:<exo_peer_port>` over the mesh (NAT-traversed, `ce-cap` gated
   by the `tunnel` ability).
3. Print the `127.0.0.1:<local>` endpoints to hand exo as **manual discovery** on this machine.

exo then forms its ring across those endpoints and does the actual model split. Because each link is
a capability-gated mesh tunnel, only authorized machines join, and no public ip:port is ever exposed.

> Cross-machine cluster formation depends on your exo version's peer port and manual-discovery
> mechanism; ce-exo provides the CE tunnels + the peer list, and you point exo at them. The CE side
> (tunnel opening, capability gating, the peer-wiring math) is unit-tested; the end-to-end multi-GPU
> exo ring is validated on real hardware, not in CI.

## The planner (`ce-exo up`)

`plan(model, nodes, opts)` advises whether a model would run as replicas (fits one node → load-
balance) or a pipeline split (too big → memory-weighted layer ring), failing loudly if the fleet is
too small. With exo as the engine, exo performs the actual split; the planner is advisory + drives
which machines to include in the cluster. With the llama.cpp backend, the same plan can drive a
`llama.cpp` RPC split (planned).

## Trust

`ce-cap` only. Abilities: `exo:infer`, `exo:host`, `exo:shard`, `exo:admin`, plus `exo:model:<prefix>`
attenuation enforced at the worker leaf. Tunnels need the `tunnel` ability on the target. Chains root
at the worker's own key or `CE_EXO_ROOTS`. Revocation = the node's on-chain set, refreshed each
minute. `--open` is single-user dev only.

## What's wired vs planned

| Area | State |
|---|---|
| Probe, registry, planner, protocol, caps | done, unit-tested |
| Worker serve loop + ce-cap auth | done |
| Mock + EngineBackend (exo / llama.cpp / Ollama / vLLM proxy) | done |
| Engine launch + supervise (`--engine-cmd`) + readiness wait | done |
| Router discovery + least-loaded dispatch + retry | done |
| OpenAI `/v1/*`, Ollama `/api/*`, web UI `/` | done |
| SDK (HTTP + optional mesh) | done |
| `cluster` — CE tunnels to stitch exo across NAT | CE side done + unit-tested; multi-GPU E2E validated on hardware |
| Streaming | synthesized from the completed response; native passthrough planned |
| Per-request payment-channel billing | planned |
| llama.cpp RPC split driven by the planner | planned |

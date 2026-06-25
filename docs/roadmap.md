# ce-exo roadmap

ce-exo is both a useful app (run LLMs across your mesh) and the proving ground for a bigger thesis:
**wrapping legacy backends in CE for seamless, secure, pay-per-use distribution** (see
[wrapping.md](wrapping.md) and [why-ce.md](why-ce.md)). The roadmap is organized so each phase that
helps ce-exo also produces reusable CE tooling.

Legend: ✅ done · 🚧 in progress · ⬜ planned

## Phase 0 — Wrap exo, expose it (✅ done, committed)
- ✅ Core: hardware probe, model registry, auto replica/pipeline planner, wire protocol, `exo:*` caps.
- ✅ Worker wraps + supervises a real engine (exo / llama.cpp / Ollama / vLLM); mock for offline tests.
- ✅ Router: OpenAI `/v1` + Ollama `/api` + web UI; mesh discovery, least-loaded dispatch.
- ✅ Lightweight SDK; CLI (`serve`, `router`, `chat`, `fleet`, `up`, `status`).
- ✅ Cross-NAT cluster stitch via CE tunnels (`cluster`); unit-tested CE side.

## Phase 1 — Seamless central distribution (🚧)
The fix for the "run a command on every machine" anti-pattern. Deploy the whole cluster from one
place; no per-host setup.
- ✅ `ce-exo deploy <model>` — launches the worker on each target **host** over the mesh via rdev
  `run` (`rdev/run/start`, gated by the `spawn` capability), with atlas-based auto node selection.
  Host execution (not a sandboxed `network=none` cell) is required because the worker must reach its
  node, open tunnels, and use the GPU.
- ⬜ Binary push: `rdev syncd` the `ce-exo` binary (+ engine bootstrap) to hosts that don't have it,
  so the only prerequisite is a running CE node + `rdev serve` + the grant.
- ⬜ Auto-form the exo ring after deploy: centrally drive each host (via rdev) to open its tunnels +
  write discovery — no `cluster` step for the user.
- ⬜ `ce-exo up <model>` becomes: plan → deploy → stitch → serve, one command, idempotent.
- ⬜ A `ce-exo grant` helper so the one-time per-host consent (issue the `spawn`/`tunnel` capability)
  is one command.

## Phase 2 — Public frontend, private infra, zero secrets (⬜)
The capability-auth superpower (see [why-ce.md](why-ce.md) §2).
- ⬜ Browser/device-side key handling: sign capability requests locally; the public UI never holds a
  secret.
- ⬜ Vault (ce-secrets) integration for engine/provider credentials — wrapped per device, never printed.
- ⬜ A hosted setup UI that configures *your* machines via relayed, scoped, revocable capabilities.
- ⬜ QR / relay pairing to enroll a new machine into your fleet with one scan (one-time consent, then
  central).

## Phase 3 — Typed SDKs in every language (⬜)
Make calling any CE app type-safe everywhere, and make wrapping cheaper.
- ⬜ Emit a machine-readable schema (JSON Schema / OpenAPI) from the wire protocol.
- ⬜ Codegen typed clients: TypeScript, Python, Go, Swift (mirroring `ce-exo-sdk`).
- ⬜ A `wrap-a-backend` SDK + template: scaffold the node-agent/transport/front-door/orchestrator for
  a new backend with the thin per-backend part stubbed.

## Phase 4 — Deployment management & observability (⬜)
Operations as platform features, visualized in the CE graph UI.
- ⬜ Declarative intent ("N replicas of model M, keep healthy") with reconcile/replace.
- ⬜ Health, restart, reroute on failure; drain/rolling updates.
- ⬜ Monitoring: per-node load, tokens/sec, latency, error rates — surfaced in the graph.
- ⬜ Error handling & alerting across the fleet.

## Phase 5 — Rent anywhere, pay per use (⬜)
- ⬜ One call to provision + deploy on a machine anywhere on the mesh (or a rented one), billed in
  credits via payment channels — per-request and per-second billing for inference.
- ⬜ Atlas/`ce-lb`-guided placement: cost-, latency-, and trust-aware node selection.

## Phase 6 — Real distributed-inference depth (⬜)
- ⬜ exo version pinning + health-aware ring reformation.
- ⬜ Optional llama.cpp RPC split driven by the core planner (for GGUF without exo).
- ⬜ Content-addressed weight distribution over CE blobs (dedup across the fleet).

---

### The throughline
Every phase advances ce-exo *and* leaves behind a CE primitive other apps reuse: central deploy,
public-frontend-secure config, typed-SDK codegen, the deployment-management plane, pay-per-use. The
goal is that wrapping the *next* legacy backend is an afternoon, because ce-exo already paid down the
platform cost.

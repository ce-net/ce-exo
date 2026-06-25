# Wrapping legacy backends in CE

ce-exo is the first worked example of a pattern that is, on its own, a reason to use CE: **take an
existing system that is hard to deploy and distribute — and wrap it so CE handles the deployment,
the networking, the identity, the secrets, and the payment.** The wrapped system keeps doing exactly
what it does well; CE supplies everything around it that used to be manual, insecure, or
cloud-locked.

exo is a perfect case. It can split a large model across machines — genuinely hard, valuable work.
But to actually use it you have to: get the machines onto the same network (or fight Tailscale/VPNs),
open ports, hand-configure peer discovery, keep processes alive, secure the API, and pay a cloud
provider per box. Every one of those is a CE primitive. So ce-exo wraps exo and the painful parts
disappear.

## What CE contributes to any wrapped backend

| The pain, before | The CE primitive that removes it |
|---|---|
| Machines can't reach each other (NAT, no public IP) | **Mesh + tunnels** — libp2p with relay/NAT traversal; peers reach each other by NodeId, never ip:port. |
| "Who is allowed to use/join this?" | **Capabilities (`ce-cap`)** — signed, attenuating grants. No device lists, no shared secrets, revocable on-chain. |
| Secrets in env files, copied to every box | **Identity + key storage** (ce-secrets/vault) — keys never leave the device; config is delivered as capabilities, not credentials. |
| "Configure the backend" needs a trusted, private admin box | **A public frontend can configure private infra** — because auth is capability/key-based, a UI hosted anywhere can drive your machines without ever holding a secret. (See [why-ce.md](why-ce.md).) |
| Provisioning + paying a cloud provider | **One mesh deploy, billed in credits** — deploy a cell to any machine on the mesh (yours, a peer's, or rented) with one call. |
| Keeping it alive, replicas, health | **Jobs + heartbeats + atlas** — the substrate already tracks liveness, capacity, and history. |
| Talking to it from another language | **Typed SDKs** (today Rust `ce-rs`; codegen for more — see roadmap). |

The wrapped backend doesn't need to know any of this exists. It speaks its normal protocol (exo:
OpenAI HTTP + its peer transport); ce-exo adapts CE to it.

## The anatomy of a wrapper (the ce-exo template)

Any "wrap a backend" app has the same four parts. ce-exo is the reference:

1. **A node agent** (`ce-exo-worker`) that runs next to the backend on each host: probes capacity,
   advertises itself on the mesh, supervises the backend process, and serves the backend's API over
   capability-gated mesh request/reply. *This is the only piece you write per backend, and it's small
   — mostly "proxy this local port, authorize the caller."*

2. **A transport stitch** (`cluster`) that wires the backend's own inter-node links over CE tunnels,
   so a backend that assumes a flat LAN works across NAT'd machines unchanged.

3. **A front door** (`ce-exo-router`) that exposes one public, capability-gated API (and a UI) for the
   whole cluster, speaking whatever dialect clients expect (OpenAI/Ollama here).

4. **A central orchestrator** (`orchestrate`) that deploys all of the above to the fleet from one
   machine with one command — `mesh-deploy` per target, billed in credits.

If you're wrapping a different backend (a database, a render farm, a game server, a Jupyter kernel),
you reuse 2–4 almost verbatim and write a thin version of 1. **Making that thin part trivial — a
`wrap-a-backend` SDK + template + codegen — is the explicit goal** (roadmap Phase: *Wrapping SDK*).

## Why this is the selling point

Distribution, identity, NAT traversal, secret handling, and billing are the things every team
rebuilds badly. CE provides them as primitives, and wrapping lets you put an *existing* engine on top
of them in an afternoon. The value isn't "CE has a mesh" — it's "your legacy backend becomes
globally deployable, secure-by-default, and pay-per-use without you writing any of that." ce-exo
proves it with exo; the next app proves it with the next backend.

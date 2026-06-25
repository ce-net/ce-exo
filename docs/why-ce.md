# Why CE — the developer experience

This document is the motivation. ce-exo is a concrete app, but the reason to build it on CE (instead
of bash + Tailscale + a cloud bill) is a set of properties CE gives you for free. They compound.

## 1. Distribution is central, not per-machine

You deploy from **one** place. You never log into the other machines. Once a node is on your mesh and
has granted you a capability — a one-time consent that is fundamental, because running code on
someone's hardware *must* be consented to — bringing an app up across the whole fleet is a single
command or API call:

```
ce-exo deploy llama-3.1-70b           # picks nodes, deploys the worker+engine to each, bills credits
```

No SSH loop, no per-host setup, no "now go run this on the other box." That last one is the
anti-pattern; if an app makes you do it, the app is wrong, not CE.

## 2. Auth that lets a *public* frontend configure *private* infra

This is the subtle, powerful one. Because authorization in CE is a **capability** (a signed,
attenuating token) and **identity is a keypair held on the device**, a frontend hosted at a public
URL can securely drive your private machines **without ever holding a secret**:

- The browser/device holds the key; it signs capability requests locally.
- The public UI never sees a credential — it only relays signed, scoped, revocable capabilities.
- Keys never leave the device; secrets are stored in the encrypted vault (ce-secrets), wrapped
  per-device, never printed.

So "set up your distributed inference cluster" can be a button on a public website that configures
machines only you control — with no trust in the website, no API keys to leak, no admin box to
secure. That is not normally possible; it is a direct consequence of capability auth + on-device keys.

## 3. Deploy on any computer in the world, pay with one call

The mesh doesn't care whose machine it is — yours, a peer's donated box, or one you rent. A directed
`mesh-deploy` runs your cell on any reachable node and bills the bid to you in credits. Renting global
compute becomes one API call, not an account signup + provider SDK + VPC + SSH key dance.

## 4. Type-safe APIs in every language

The wire protocol is typed. Today the Rust SDK (`ce-rs`, and ce-exo's own `ce-exo-sdk`) gives you
typed clients. The protocol is structured so the **same types can be code-generated for TypeScript,
Python, Go, Swift, …** from one schema — so calling a CE app from any language is type-safe by
default, not hand-rolled JSON. Building that codegen is tooling that makes every future wrapper
cheaper (roadmap: *Typed multi-language SDKs*).

## 5. Operations you don't have to build

Replication, health, monitoring, error handling, and a live view of the whole system **in the CE
graph UI** — these are platform features, not things each app reinvents. An app declares what it
wants ("3 replicas of this model, keep them healthy"); the platform places, watches, reroutes, and
shows it to you. (Roadmap: *Deployment management*.)

## The pitch, in one line

> Take any backend, wrap it once, and get global, secure-by-default, pay-per-use distribution —
> deployed and managed from one place, callable type-safely from anywhere, configurable from a public
> UI that never holds your keys.

CE is the substrate that makes that true. ce-exo is the proof for LLMs; the [wrapping guide](wrapping.md)
is how you do it for the next backend; the [roadmap](roadmap.md) is where it's going.

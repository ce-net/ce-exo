# ce-exo mesh protocol (v1)

All ce-exo coordination rides the CE mesh via `ce-rs` (`request`/`reply`, `send_message`, `publish`).
Payloads are JSON (debuggable; bincode is a later optimization). The sender NodeId on every message
is cryptographically authenticated by the local CE node, so authorization decisions trust `req.from`.

`PROTOCOL_VERSION = 1`. Workers advertise it in `WorkerStatus`; a router refuses incompatible peers.

## Topics

| Topic | Pattern | Direction | Payload → Reply |
|---|---|---|---|
| `exo/status/v1` | request/reply | router → worker | `StatusRequest` → `WorkerStatus` |
| `exo/infer/v1` | request/reply | router/SDK → worker (replica or pipeline head) | `InferRequest` → `InferResponse` |
| `exo/stream/v1/<request_id>` | directed send | worker → requester | `TokenChunk` (per token) |
| `exo/plan/v1` | pub/sub | admin → all stages | `PipelinePlanMsg` |
| `exo/act/v1` | directed send | stage *k* → stage *k+1* | `Activation` |

Service discovery (DHT, via `ce-rs` `advertise_service`/`find_service`):
- `exo-host` — every worker advertises this.
- `exo-model:<id>` — a worker advertises one per resident model, so the router can find servers for a
  specific model directly.

## Messages

### InferRequest → InferResponse
```jsonc
// InferRequest
{
  "request_id": "exo-<nanos>-<n>",
  "model": "llama3.1-8b-q4",
  "messages": [{ "role": "user", "content": "hi" }],
  "params": { "max_tokens": 512, "temperature": 0.7, "top_p": null, "stop": [] },
  "stream": false,
  "cap_chain_hex": ""           // hex bincode Vec<SignedCapability>; empty relies on worker --open
}
// InferResponse
{
  "request_id": "exo-...",
  "model": "llama3.1-8b-q4",
  "text": "...",
  "prompt_tokens": 3,
  "completion_tokens": 12,
  "finish_reason": "stop",      // stop | length | error
  "error": null
}
```

### WorkerStatus
```jsonc
{
  "node_id": "<64-hex>",
  "protocol_version": 1,
  "backend": "mock",            // or "llama.cpp"
  "cpu_cores": 8,
  "has_gpu": true,
  "budget_mb": 16800,           // MB of weights this worker holds resident
  "models_ready": ["mock-tiny"],
  "active_requests": 0          // load signal for least-loaded selection
}
```

### TokenChunk (streaming)
```jsonc
{ "request_id": "exo-...", "seq": 0, "token": "Hello ", "done": false }
```

### PipelinePlanMsg / Activation (pipeline mode)
```jsonc
// PipelinePlanMsg — broadcast so each stage learns its layer range
{
  "plan_id": "...",
  "placement": { "model_id": "...", "mode": "pipeline",
                 "stages": [{ "node_id": "...", "layer_lo": 0, "layer_hi": 39,
                              "weight_shard_cid": null }] },
  "total_layers": 80,
  "issuer": "<64-hex>",
  "cap_chain_hex": "..."        // proves exo:admin
}
// Activation — the only payload that crosses the wire between stages (~KB/token)
{ "plan_id": "...", "request_id": "...", "position": 0, "hidden": [/* f32 hidden state */],
  "emitted": null }             // last stage sets `emitted` to the sampled token
```

## Authorization

Each privileged request carries a hex `ce-cap` chain (`cap_chain_hex`). The worker calls
`ce_cap::authorize(self_id, accepted_roots, self_tags, now, requester, action, chain, is_revoked)`
with `action` = `exo:infer` (inference) or `exo:shard` (stage join), then enforces any
`exo:model:<prefix>` restriction on the leaf against the request's model id. Roots: the worker's own
key (always) plus `CE_EXO_ROOTS`. Revocation: the node's on-chain set, refreshed each minute.

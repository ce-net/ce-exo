//! ce-exo router — discovery, dispatch, and placement over the CE mesh.
//!
//! The router is the **one public surface** of a ce-exo cluster (the mesh-app rule: a cluster
//! exposes exactly one HTTP domain). It speaks the OpenAI and Ollama HTTP dialects so existing tools
//! (OpenWebUI, `ollama`-aware clients, SDKs) work unchanged, and underneath it:
//!
//! 1. **discovers** workers via the DHT (`exo-host` / `exo-model:<id>` service records) and reads
//!    each one's [`WorkerStatus`] over the mesh;
//! 2. **picks** the least-loaded worker that serves the requested model (GPU and budget break ties);
//! 3. **dispatches** an [`InferRequest`] via `ce-rs` `request`/`reply`, retrying the next candidate
//!    on failure;
//! 4. **plans** placements ([`Router::plan_model`]) with the core auto-mode planner for `exo up`.
//!
//! Authorization: the router presents its configured `ce-cap` chain (`--grant`) to every worker;
//! with no grant it relies on workers running in `--open` dev mode.

pub mod cluster;
pub mod http;
pub mod orchestrate;

use anyhow::{anyhow, Context, Result};
use ce_exo_core::plan::{plan, NodeCap, Placement, PlanOpts};
use ce_exo_core::proto::{
    InferRequest, InferResponse, SampleParams, StatusRequest, WorkerStatus, INFER_TOPIC,
    STATUS_TOPIC,
};
use ce_exo_core::registry::Registry;
use ce_exo_core::proto::ChatMessage;
use ce_rs::CeClient;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Default per-request mesh timeout.
const INFER_TIMEOUT_MS: u64 = 120_000;
const STATUS_TIMEOUT_MS: u64 = 4_000;

/// The router state, shared across HTTP handlers.
pub struct Router {
    ce: CeClient,
    registry: Registry,
    /// Hex `ce-cap` chain presented to workers (empty = rely on worker `--open`).
    cap_chain_hex: String,
    seq: AtomicU64,
}

impl Router {
    pub fn new(ce: CeClient, registry: Registry, cap_chain_hex: String) -> Arc<Self> {
        Arc::new(Router { ce, registry, cap_chain_hex, seq: AtomicU64::new(0) })
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// A unique request id: unix-nanos plus a per-process counter (no rng dependency).
    fn next_request_id(&self) -> String {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("exo-{nanos:x}-{n:x}")
    }

    /// Every worker advertising `exo-host`, with its live status (best-effort; unreachable workers
    /// are dropped).
    pub async fn fleet(&self) -> Result<Vec<WorkerStatus>> {
        let nodes = self.ce.find_service("exo-host").await.unwrap_or_default();
        Ok(self.status_of(&nodes).await)
    }

    /// Workers that currently serve `model`, least-loaded first.
    pub async fn workers_for_model(&self, model: &str) -> Result<Vec<WorkerStatus>> {
        // Prefer the per-model service record; fall back to scanning the whole fleet.
        let mut nodes = self.ce.find_service(&format!("exo-model:{model}")).await.unwrap_or_default();
        if nodes.is_empty() {
            nodes = self.ce.find_service("exo-host").await.unwrap_or_default();
        }
        let mut serving: Vec<WorkerStatus> = self
            .status_of(&nodes)
            .await
            .into_iter()
            .filter(|s| s.models_ready.iter().any(|m| m == model))
            .collect();
        // Least-loaded first; GPU then budget break ties.
        serving.sort_by(|a, b| {
            a.active_requests
                .cmp(&b.active_requests)
                .then(b.has_gpu.cmp(&a.has_gpu))
                .then(b.budget_mb.cmp(&a.budget_mb))
        });
        Ok(serving)
    }

    async fn status_of(&self, node_ids: &[String]) -> Vec<WorkerStatus> {
        let payload = serde_json::to_vec(&StatusRequest {}).unwrap_or_default();
        let futs = node_ids.iter().map(|id| {
            let payload = payload.clone();
            async move {
                let bytes = self.ce.request(id, STATUS_TOPIC, &payload, STATUS_TIMEOUT_MS).await.ok()?;
                serde_json::from_slice::<WorkerStatus>(&bytes).ok()
            }
        });
        futures_util::future::join_all(futs).await.into_iter().flatten().collect()
    }

    /// Run a chat completion: discover, pick, dispatch, retry. Returns the worker's response.
    pub async fn chat(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
        params: SampleParams,
    ) -> Result<InferResponse> {
        let candidates = self.workers_for_model(model).await?;
        if candidates.is_empty() {
            return Err(anyhow!(
                "no worker is serving model '{model}' — start one with `ce-exo serve --model {model}`"
            ));
        }

        let mut last_err = anyhow!("no candidate succeeded");
        for w in &candidates {
            let req = InferRequest {
                request_id: self.next_request_id(),
                model: model.to_string(),
                messages: messages.clone(),
                params: params.clone(),
                stream: false,
                cap_chain_hex: self.cap_chain_hex.clone(),
            };
            let payload = serde_json::to_vec(&req).unwrap_or_default();
            match self.ce.request(&w.node_id, INFER_TOPIC, &payload, INFER_TIMEOUT_MS).await {
                Ok(bytes) => match serde_json::from_slice::<InferResponse>(&bytes) {
                    Ok(resp) if resp.error.is_none() => return Ok(resp),
                    Ok(resp) => {
                        last_err = anyhow!(resp.error.unwrap_or_else(|| "worker error".into()));
                    }
                    Err(e) => last_err = anyhow!("decoding worker reply: {e}"),
                },
                Err(e) => {
                    tracing::warn!(node = %w.node_id, error = %e, "dispatch failed; trying next worker");
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    /// Compute a placement for `model_id` across the live fleet (for `ce-exo up`). Pure planner over
    /// discovered capacity; does not deploy.
    pub async fn plan_model(&self, model_id: &str, opts: &PlanOpts) -> Result<Placement> {
        let model = self.registry.get(model_id)?;
        let fleet = self.fleet().await?;
        if fleet.is_empty() {
            return Err(anyhow!("no workers online; start workers before planning"));
        }
        let mut caps = Vec::with_capacity(fleet.len());
        for w in &fleet {
            let history = self.ce.history(&w.node_id).await.map(|h| h.delivered_work()).unwrap_or(0);
            caps.push(NodeCap {
                node_id: w.node_id.clone(),
                budget_mb: w.budget_mb,
                cpu_cores: w.cpu_cores,
                has_gpu: w.has_gpu,
                history,
            });
        }
        plan(model, &caps, opts)
    }
}

/// Bind `addr` and serve the router's OpenAI/Ollama HTTP front door until the process exits. The one
/// public surface of a ce-exo cluster.
pub async fn serve(router: Arc<Router>, addr: &str) -> Result<()> {
    let app = http::build_app(router);
    let socket: std::net::SocketAddr = addr.parse().with_context(|| format!("invalid bind '{addr}'"))?;
    let listener = tokio::net::TcpListener::bind(socket).await.with_context(|| format!("binding {socket}"))?;
    tracing::info!(%socket, "ce-exo router listening (OpenAI: /v1, Ollama: /api)");
    axum::serve(listener, app).await.context("serving HTTP")?;
    Ok(())
}

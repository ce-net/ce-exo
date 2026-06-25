//! ce-exo worker library — the per-node inference service.
//!
//! A [`Worker`] wraps a [`Backend`] and answers two mesh request topics:
//! [`STATUS_TOPIC`](ce_exo_core::proto::STATUS_TOPIC) (who am I, what do I serve) and
//! [`INFER_TOPIC`](ce_exo_core::proto::INFER_TOPIC) (run a completion). Every infer request is
//! authorized by a `ce-cap` chain (unless the worker runs in `--open` dev mode), and may be streamed
//! back to the requester token-by-token. The worker advertises itself and each resident model over
//! the DHT so the router can discover it.

mod backend;
pub mod launch;
pub use backend::{Backend, EngineBackend, MockBackend, EXO_DEFAULT_URL, LLAMA_DEFAULT_URL};
pub use launch::EngineProcess;

use anyhow::{anyhow, Result};
use ce_cap::decode_chain_bytes;
use ce_exo_core::proto::{
    stream_topic, InferRequest, InferResponse, TokenChunk, WorkerStatus, INFER_TOPIC, STATUS_TOPIC,
};
use ce_exo_core::{caps, HardwareProbe};
use ce_identity::NodeId;
use ce_rs::serve::{Handler, Request};
use ce_rs::CeClient;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Worker policy: trust roots and the dev-open escape hatch.
pub struct WorkerConfig {
    /// Skip capability enforcement entirely (single-user dev / self-host). Never enable on a shared
    /// or public worker.
    pub open: bool,
    /// Root keys this worker honors in addition to its own (chains rooting at the worker's own key
    /// are always honored by `ce-cap`).
    pub accepted_roots: Vec<NodeId>,
}

/// A running inference worker.
pub struct Worker {
    ce: CeClient,
    backend: Backend,
    probe: HardwareProbe,
    self_id: NodeId,
    self_id_hex: String,
    self_tags: Vec<String>,
    cfg: WorkerConfig,
    revoked: Mutex<HashSet<(String, u64)>>,
    active: AtomicU32,
}

impl Worker {
    /// Build a worker bound to a local CE node (`ce`), discovering its own NodeId from `/status`.
    pub async fn new(
        ce: CeClient,
        backend: Backend,
        probe: HardwareProbe,
        cfg: WorkerConfig,
    ) -> Result<Arc<Self>> {
        let status = ce.status().await?;
        let self_id = parse_node_id(&status.node_id)?;
        let self_tags = probe.service_tags();
        Ok(Arc::new(Worker {
            ce,
            backend,
            probe,
            self_id,
            self_id_hex: status.node_id,
            self_tags,
            cfg,
            revoked: Mutex::new(HashSet::new()),
            active: AtomicU32::new(0),
        }))
    }

    pub fn node_id_hex(&self) -> &str {
        &self.self_id_hex
    }

    /// This worker's self-description.
    pub fn status(&self) -> WorkerStatus {
        WorkerStatus {
            node_id: self.self_id_hex.clone(),
            protocol_version: ce_exo_core::PROTOCOL_VERSION,
            backend: self.backend.name().to_string(),
            cpu_cores: self.probe.cpu_cores,
            has_gpu: self.probe.has_gpu,
            budget_mb: self.probe.weight_budget_mb(),
            models_ready: self.backend.models_ready(),
            active_requests: self.active.load(Ordering::Relaxed),
        }
    }

    /// Verify a chain authorizes `INFER` on this worker for `model`. `Ok(())` in open mode.
    fn authorize_infer(&self, from_hex: &str, chain_hex: &str, model: &str) -> Result<(), String> {
        if self.cfg.open {
            return Ok(());
        }
        let requester = parse_node_id(from_hex).map_err(|e| e.to_string())?;
        let bytes = hex::decode(chain_hex.trim())
            .map_err(|_| "capability chain is not valid hex".to_string())?;
        let chain = decode_chain_bytes(&bytes).map_err(|e| e.to_string())?;
        let revoked = self.revoked.lock().expect("revoked lock").clone();
        let is_revoked = |issuer: &NodeId, nonce: u64| revoked.contains(&(hex::encode(issuer), nonce));
        ce_cap::authorize(
            &self.self_id,
            &self.cfg.accepted_roots,
            &self.self_tags,
            unix_now(),
            &requester,
            caps::INFER,
            &chain,
            &is_revoked,
        )?;
        // Per-model attenuation, enforced at the leaf.
        let leaf = chain.last().ok_or_else(|| "empty capability chain".to_string())?;
        if !caps::model_allowed(leaf.cap.abilities.iter().map(String::as_str), model) {
            return Err(format!("capability does not authorize model '{model}'"));
        }
        Ok(())
    }

    async fn handle_infer(&self, req: Request) -> Vec<u8> {
        let parsed: InferRequest = match serde_json::from_slice(&req.payload) {
            Ok(r) => r,
            Err(e) => {
                return enc(&InferResponse::error("unknown", "unknown", format!("bad request: {e}")))
            }
        };
        if let Err(e) = self.authorize_infer(&req.from, &parsed.cap_chain_hex, &parsed.model) {
            return enc(&InferResponse::error(
                &parsed.request_id,
                &parsed.model,
                format!("unauthorized: {e}"),
            ));
        }
        if !self.backend.serves(&parsed.model) {
            return enc(&InferResponse::error(
                &parsed.request_id,
                &parsed.model,
                format!("model '{}' is not served by this worker", parsed.model),
            ));
        }

        self.active.fetch_add(1, Ordering::Relaxed);
        let result = self.run_infer(&req.from, &parsed).await;
        self.active.fetch_sub(1, Ordering::Relaxed);

        match result {
            Ok(resp) => enc(&resp),
            Err(e) => enc(&InferResponse::error(&parsed.request_id, &parsed.model, e.to_string())),
        }
    }

    /// Run a completion and, if requested, stream tokens back to the requester before returning the
    /// terminal response.
    async fn run_infer(&self, requester_hex: &str, req: &InferRequest) -> Result<InferResponse> {
        let resp = self.backend.infer(req).await?;
        if req.stream {
            let topic = stream_topic(&req.request_id);
            let tokens: Vec<&str> = resp.text.split_inclusive(char::is_whitespace).collect();
            if tokens.is_empty() {
                let chunk = TokenChunk {
                    request_id: req.request_id.clone(),
                    seq: 0,
                    token: String::new(),
                    done: true,
                };
                let _ = self.ce.send_message(requester_hex, &topic, &enc(&chunk)).await;
            } else {
                let last = tokens.len() - 1;
                for (i, tok) in tokens.iter().enumerate() {
                    let chunk = TokenChunk {
                        request_id: req.request_id.clone(),
                        seq: i as u32,
                        token: (*tok).to_string(),
                        done: i == last,
                    };
                    let _ = self.ce.send_message(requester_hex, &topic, &enc(&chunk)).await;
                }
            }
        }
        Ok(resp)
    }

    /// Advertise this worker and each resident model on the DHT (call periodically; records expire).
    async fn advertise(&self) {
        if let Err(e) = self.ce.advertise_service("exo-host").await {
            tracing::debug!(error = %e, "advertise exo-host failed");
        }
        for m in self.backend.models_ready() {
            let _ = self.ce.advertise_service(&format!("exo-model:{m}")).await;
        }
    }

    async fn refresh_revoked(&self) {
        if let Ok(set) = self.ce.revoked().await {
            *self.revoked.lock().expect("revoked lock") = set.into_iter().collect();
        }
    }
}

impl Handler for Worker {
    async fn handle(&self, req: Request) -> Vec<u8> {
        match req.topic.as_str() {
            STATUS_TOPIC => enc(&self.status()),
            INFER_TOPIC => self.handle_infer(req).await,
            other => {
                tracing::debug!(topic = other, "worker ignoring unknown topic");
                Vec::new()
            }
        }
    }
}

/// Run the worker until ctrl-c: a background advertise/refresh loop plus the mesh serve loop.
pub async fn run(worker: Arc<Worker>) -> Result<()> {
    worker.advertise().await;
    worker.refresh_revoked().await;
    tracing::info!(
        node = worker.node_id_hex(),
        backend = worker.backend.name(),
        models = ?worker.backend.models_ready(),
        budget_mb = worker.probe.weight_budget_mb(),
        "ce-exo worker online"
    );

    let bg = worker.clone();
    let bg_task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            bg.advertise().await;
            bg.refresh_revoked().await;
        }
    });

    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutdown signal received");
    };
    let res = ce_rs::serve::serve(&worker.ce, &[INFER_TOPIC, STATUS_TOPIC], worker.as_ref(), shutdown).await;
    bg_task.abort();
    res
}

/// Decode a 64-hex NodeId into the `[u8; 32]` form `ce-cap` expects.
pub fn parse_node_id(hexs: &str) -> Result<NodeId> {
    let bytes = hex::decode(hexs.trim()).map_err(|_| anyhow!("node id is not valid hex"))?;
    let arr: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("node id must be 32 bytes, got {}", bytes.len()))?;
    Ok(arr)
}

fn enc<T: serde::Serialize>(v: &T) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_default()
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

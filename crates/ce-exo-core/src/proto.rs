//! The ce-exo wire protocol — what flows over the CE mesh between router, workers, and pipeline
//! stages. Every message is JSON (matching the `ce-rs` `request`/`reply` hex-payload convention) so
//! it stays debuggable; bincode is a later optimization.
//!
//! ## Topics (CE mesh `request`/`reply` + pub/sub)
//!
//! - [`INFER_TOPIC`] — directed request/reply: router → worker (replica) or → pipeline head. Carries
//!   an [`InferRequest`], returns an [`InferResponse`]. Token streaming is delivered as a sequence of
//!   fire-and-forget [`TokenChunk`] messages on [`stream_topic`] keyed by the request id, terminated
//!   by a final [`InferResponse`].
//! - [`STATUS_TOPIC`] — request/reply: ask a worker what it is and what it's serving
//!   ([`StatusRequest`] → [`WorkerStatus`]).
//! - [`PLAN_TOPIC`] — pub/sub: the admin broadcasts a signed [`PipelinePlanMsg`] so every stage
//!   learns the ring it belongs to.
//! - [`ACT_TOPIC`] — directed fire-and-forget: pipeline stage *k* → stage *k+1*, carrying an
//!   [`Activation`] (the boundary hidden state for one decode step). Only this crosses the wire in
//!   pipeline mode.

use crate::plan::Placement;
use serde::{Deserialize, Serialize};

/// Request/reply: run inference (replica worker or pipeline head).
pub const INFER_TOPIC: &str = "exo/infer/v1";
/// Request/reply: query a worker's identity + loaded models.
pub const STATUS_TOPIC: &str = "exo/status/v1";
/// Pub/sub: signed pipeline plans.
pub const PLAN_TOPIC: &str = "exo/plan/v1";
/// Directed: inter-stage activation tensors.
pub const ACT_TOPIC: &str = "exo/act/v1";

/// The directed token-stream topic for one request id (so a worker can fan tokens back to the
/// router without a reply channel). The router subscribes before issuing the request.
pub fn stream_topic(request_id: &str) -> String {
    format!("exo/stream/v1/{request_id}")
}

/// A chat message in the OpenAI shape (the lingua franca ce-exo speaks end to end).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// Sampling controls. Sensible defaults so callers can send `{}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SampleParams {
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub stop: Vec<String>,
}

fn default_max_tokens() -> u32 {
    512
}
fn default_temperature() -> f32 {
    0.7
}

impl Default for SampleParams {
    fn default() -> Self {
        SampleParams {
            max_tokens: default_max_tokens(),
            temperature: default_temperature(),
            top_p: None,
            stop: Vec::new(),
        }
    }
}

/// An inference request carried over the mesh to a worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InferRequest {
    /// Caller-chosen unique id; also names the token stream topic.
    pub request_id: String,
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub params: SampleParams,
    /// Stream tokens on [`stream_topic`] as they are produced.
    #[serde(default)]
    pub stream: bool,
    /// Hex-encoded `ce-cap` capability chain authorizing this call (bincode `Vec<SignedCapability>`).
    /// Empty means "rely on a chain rooted at the worker's own key" (self-host / dev).
    #[serde(default)]
    pub cap_chain_hex: String,
}

/// The terminal reply to an [`InferRequest`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InferResponse {
    pub request_id: String,
    pub model: String,
    /// Full text (non-streaming), or the concatenation already delivered via [`TokenChunk`]s.
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    /// `stop`, `length`, or an error string.
    pub finish_reason: String,
    #[serde(default)]
    pub error: Option<String>,
}

impl InferResponse {
    pub fn error(request_id: &str, model: &str, msg: impl Into<String>) -> Self {
        InferResponse {
            request_id: request_id.into(),
            model: model.into(),
            text: String::new(),
            prompt_tokens: 0,
            completion_tokens: 0,
            finish_reason: "error".into(),
            error: Some(msg.into()),
        }
    }
}

/// One streamed token (fire-and-forget on [`stream_topic`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenChunk {
    pub request_id: String,
    /// Monotonic index from 0; lets the router order/dedup.
    pub seq: u32,
    pub token: String,
    /// True on the last chunk (the [`InferResponse`] follows / accompanies it).
    #[serde(default)]
    pub done: bool,
}

/// Ask a worker who it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusRequest {}

/// A worker's self-description.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerStatus {
    pub node_id: String,
    pub protocol_version: u32,
    /// Backend name, e.g. `llama.cpp` or `mock`.
    pub backend: String,
    pub cpu_cores: u32,
    pub has_gpu: bool,
    pub budget_mb: u64,
    /// Model ids currently resident and ready to serve.
    pub models_ready: Vec<String>,
    /// In-flight request count (load signal for the router / ce-lb).
    pub active_requests: u32,
}

/// A signed pipeline plan broadcast to all stages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelinePlanMsg {
    pub plan_id: String,
    pub placement: Placement,
    /// Total layers in the model (so a stage can sanity-check the ring).
    pub total_layers: u32,
    /// Issuer node id (the admin); the chain is verified by stages before they join.
    pub issuer: String,
    /// Hex `ce-cap` chain proving the issuer may push plans (`exo:admin`).
    #[serde(default)]
    pub cap_chain_hex: String,
}

/// The boundary activation handed from one pipeline stage to the next for a single decode step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Activation {
    pub plan_id: String,
    pub request_id: String,
    /// Decode position in the sequence.
    pub position: u32,
    /// The hidden-state vector leaving this stage (length = model hidden dim). The only weight-free,
    /// ~KB/token payload that crosses the wire in pipeline mode.
    pub hidden: Vec<f32>,
    /// Set by the final stage: the sampled token id/text to emit (no further forwarding).
    #[serde(default)]
    pub emitted: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_request_roundtrips_with_defaults() {
        let json = r#"{"request_id":"r1","model":"m","messages":[{"role":"user","content":"hi"}]}"#;
        let req: InferRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.params.max_tokens, 512);
        assert_eq!(req.params.temperature, 0.7);
        assert!(!req.stream);
        assert!(req.cap_chain_hex.is_empty());
    }

    #[test]
    fn stream_topic_is_per_request() {
        assert_eq!(stream_topic("abc"), "exo/stream/v1/abc");
    }

    #[test]
    fn error_response_helper() {
        let r = InferResponse::error("r1", "m", "boom");
        assert_eq!(r.finish_reason, "error");
        assert_eq!(r.error.as_deref(), Some("boom"));
    }
}

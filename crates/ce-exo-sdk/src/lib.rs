//! # ce-exo-sdk — the lightweight client for talking to a ce-exo cluster
//!
//! A thin wrapper over a ce-exo router's OpenAI-compatible HTTP API. The whole point is to be small:
//! construct an [`ExoClient`], call [`chat`](ExoClient::chat) or
//! [`chat_stream`](ExoClient::chat_stream), get text back. No mesh, no node, no auth ceremony unless
//! you want it.
//!
//! ```no_run
//! use ce_exo_sdk::{ExoClient, msg};
//! # async fn demo() -> anyhow::Result<()> {
//! let exo = ExoClient::new("http://127.0.0.1:8088");
//! let reply = exo.chat("llama3.1-8b-q4", vec![msg::user("Explain CE in one line")]).await?;
//! println!("{reply}");
//! # Ok(()) }
//! ```
//!
//! With the `mesh` feature enabled, [`mesh::dispatch`] talks **directly to a worker node** over the
//! CE mesh (no router hop) for callers that already hold a [`ce_rs::CeClient`].

use anyhow::{anyhow, Context, Result};
pub use ce_exo_core::proto::{ChatMessage, SampleParams};
use serde_json::{json, Value};

/// Convenience constructors for chat messages.
pub mod msg {
    use super::ChatMessage;
    pub fn system(content: impl Into<String>) -> ChatMessage {
        ChatMessage { role: "system".into(), content: content.into() }
    }
    pub fn user(content: impl Into<String>) -> ChatMessage {
        ChatMessage { role: "user".into(), content: content.into() }
    }
    pub fn assistant(content: impl Into<String>) -> ChatMessage {
        ChatMessage { role: "assistant".into(), content: content.into() }
    }
}

/// A client for a ce-exo router.
#[derive(Debug, Clone)]
pub struct ExoClient {
    base: String,
    http: reqwest::Client,
    api_key: Option<String>,
}

impl ExoClient {
    /// Client for a router at `base_url` (e.g. `http://127.0.0.1:8088`).
    pub fn new(base_url: impl Into<String>) -> Self {
        ExoClient {
            base: base_url.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
            api_key: None,
        }
    }

    /// Attach a bearer token (sent as `Authorization: Bearer ...`) for routers behind an auth proxy.
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        let mut b = self.http.post(format!("{}{path}", self.base));
        if let Some(k) = &self.api_key {
            b = b.bearer_auth(k);
        }
        b
    }

    /// List the model ids the cluster advertises (`GET /v1/models`).
    pub async fn models(&self) -> Result<Vec<String>> {
        let mut b = self.http.get(format!("{}/v1/models", self.base));
        if let Some(k) = &self.api_key {
            b = b.bearer_auth(k);
        }
        let v: Value = b.send().await?.error_for_status()?.json().await?;
        Ok(v["data"]
            .as_array()
            .map(|a| a.iter().filter_map(|m| m["id"].as_str().map(String::from)).collect())
            .unwrap_or_default())
    }

    /// Run a chat completion and return the assistant's text. For control over sampling use
    /// [`chat_with`](Self::chat_with).
    pub async fn chat(&self, model: &str, messages: Vec<ChatMessage>) -> Result<String> {
        self.chat_with(model, messages, &SampleParams::default()).await
    }

    /// Run a chat completion with explicit sampling parameters.
    pub async fn chat_with(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
        params: &SampleParams,
    ) -> Result<String> {
        let body = request_body(model, &messages, params, false);
        let v: Value = self
            .post("/v1/chat/completions")
            .json(&body)
            .send()
            .await
            .context("calling router")?
            .error_for_status()
            .context("router returned an error")?
            .json()
            .await
            .context("decoding router response")?;
        v["choices"][0]["message"]["content"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| anyhow!("router response missing choices[0].message.content: {v}"))
    }

    /// Stream a chat completion, invoking `on_token` for each text delta as it arrives. Returns the
    /// full assembled text. Consumes the router's SSE stream.
    pub async fn chat_stream(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
        params: &SampleParams,
        mut on_token: impl FnMut(&str),
    ) -> Result<String> {
        use futures_util::StreamExt as _;
        let body = request_body(model, &messages, params, true);
        let resp = self
            .post("/v1/chat/completions")
            .json(&body)
            .send()
            .await
            .context("calling router")?
            .error_for_status()
            .context("router returned an error")?;

        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut full = String::new();
        while let Some(chunk) = stream.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk?));
            // SSE frames are separated by blank lines; events are `data: <json>` or `data: [DONE]`.
            while let Some(pos) = buf.find("\n\n") {
                let frame = buf[..pos].to_string();
                buf.drain(..pos + 2);
                for line in frame.lines() {
                    let Some(data) = line.strip_prefix("data: ") else { continue };
                    let data = data.trim();
                    if data == "[DONE]" {
                        return Ok(full);
                    }
                    if let Ok(v) = serde_json::from_str::<Value>(data) {
                        if let Some(tok) = v["choices"][0]["delta"]["content"].as_str() {
                            on_token(tok);
                            full.push_str(tok);
                        }
                    }
                }
            }
        }
        Ok(full)
    }
}

fn request_body(model: &str, messages: &[ChatMessage], params: &SampleParams, stream: bool) -> Value {
    let messages: Vec<Value> =
        messages.iter().map(|m| json!({ "role": m.role, "content": m.content })).collect();
    let mut body = json!({
        "model": model,
        "messages": messages,
        "max_tokens": params.max_tokens,
        "temperature": params.temperature,
        "stream": stream,
    });
    if let Some(top_p) = params.top_p {
        body["top_p"] = json!(top_p);
    }
    if !params.stop.is_empty() {
        body["stop"] = json!(params.stop);
    }
    body
}

/// Direct-to-worker dispatch over the CE mesh (no router hop). Requires the `mesh` feature.
#[cfg(feature = "mesh")]
pub mod mesh {
    use super::*;
    use ce_exo_core::proto::{InferRequest, InferResponse, INFER_TOPIC};
    use ce_rs::CeClient;

    /// Send an inference request straight to a specific worker `node_id` over the mesh and return its
    /// response. `cap_chain_hex` authorizes the call (empty if the worker runs `--open`).
    pub async fn dispatch(
        ce: &CeClient,
        node_id: &str,
        model: &str,
        messages: Vec<ChatMessage>,
        params: SampleParams,
        cap_chain_hex: &str,
        timeout_ms: u64,
    ) -> Result<InferResponse> {
        let req = InferRequest {
            request_id: format!("sdk-{}", messages.len()),
            model: model.to_string(),
            messages,
            params,
            stream: false,
            cap_chain_hex: cap_chain_hex.to_string(),
        };
        let payload = serde_json::to_vec(&req)?;
        let bytes = ce.request(node_id, INFER_TOPIC, &payload, timeout_ms).await?;
        let resp: InferResponse = serde_json::from_slice(&bytes)?;
        if let Some(err) = &resp.error {
            return Err(anyhow!("worker error: {err}"));
        }
        Ok(resp)
    }
}

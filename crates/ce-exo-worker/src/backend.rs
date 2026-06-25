//! Inference backends — what generates tokens on a worker.
//!
//! The worker does **not** implement inference. It wraps a real engine and contributes the CE layer
//! (mesh transport, capability auth, discovery, economy). Two variants:
//!
//! - [`MockBackend`] — deterministic, dependency-free generation, so the whole CE path (discovery →
//!   dispatch → stream) is testable on any machine with no GPU and no engine.
//! - [`EngineBackend`] — forwards to a local **OpenAI-compatible** server on loopback. That is the
//!   integration point for a real engine:
//!     - **exo** (`exo`'s ChatGPT API, default port 52415) — the headline: exo splits a model across
//!       machines; CE tunnels stitch those machines together across NAT (see `ce-exo-router::cluster`).
//!     - **llama.cpp** (`llama-server`), **Ollama**, **vLLM** — all speak the same dialect.
//!
//! The worker can also *launch and supervise* the engine process (see `launch` in the worker), so
//! `ce-exo serve --backend exo --launch` brings exo up, joins it to the mesh, and tears it down on
//! exit.

use anyhow::{anyhow, Context, Result};
use ce_exo_core::proto::{InferRequest, InferResponse};

/// Default loopback endpoints for the engines ce-exo knows about.
pub const EXO_DEFAULT_URL: &str = "http://127.0.0.1:52415";
pub const LLAMA_DEFAULT_URL: &str = "http://127.0.0.1:8081";

/// A worker's generation backend.
pub enum Backend {
    Mock(MockBackend),
    Engine(EngineBackend),
}

impl Backend {
    pub fn name(&self) -> &str {
        match self {
            Backend::Mock(_) => "mock",
            Backend::Engine(b) => &b.label,
        }
    }

    /// Model ids this backend can currently serve.
    pub fn models_ready(&self) -> Vec<String> {
        match self {
            Backend::Mock(b) => b.models.clone(),
            Backend::Engine(b) => b.models.clone(),
        }
    }

    pub fn serves(&self, model: &str) -> bool {
        self.models_ready().iter().any(|m| m == model)
    }

    /// Generate a completion for `req`.
    pub async fn infer(&self, req: &InferRequest) -> Result<InferResponse> {
        match self {
            Backend::Mock(b) => Ok(b.infer(req)),
            Backend::Engine(b) => b.infer(req).await,
        }
    }
}

/// Deterministic, offline generation for testing and demos.
pub struct MockBackend {
    pub models: Vec<String>,
}

impl MockBackend {
    pub fn new(models: Vec<String>) -> Self {
        MockBackend { models }
    }

    pub fn infer(&self, req: &InferRequest) -> InferResponse {
        let last_user = req
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .map(|m| m.content.trim())
            .unwrap_or("");
        let prompt_tokens: u32 = req.messages.iter().map(|m| word_count(&m.content)).sum();

        let want = req.params.max_tokens.clamp(1, 64) as usize;
        let body = format!(
            "ce-exo[{}] received {} message(s); responding to: \"{}\"",
            req.model,
            req.messages.len(),
            truncate(last_user, 160)
        );
        let text: String = body.split_whitespace().take(want).collect::<Vec<_>>().join(" ");
        let completion_tokens = word_count(&text);

        InferResponse {
            request_id: req.request_id.clone(),
            model: req.model.clone(),
            text,
            prompt_tokens,
            completion_tokens,
            finish_reason: "stop".into(),
            error: None,
        }
    }
}

/// Forwards to a local OpenAI-compatible server (`POST {base}/v1/chat/completions`).
pub struct EngineBackend {
    /// Human label surfaced in status (`exo`, `llama.cpp`, ...).
    pub label: String,
    pub models: Vec<String>,
    pub base_url: String,
    http: reqwest::Client,
}

impl EngineBackend {
    pub fn new(label: impl Into<String>, models: Vec<String>, base_url: impl Into<String>) -> Self {
        EngineBackend {
            label: label.into(),
            models,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
        }
    }

    /// An exo-backed worker (exo's ChatGPT API).
    pub fn exo(models: Vec<String>, base_url: impl Into<String>) -> Self {
        Self::new("exo", models, base_url)
    }

    /// A llama.cpp / Ollama / vLLM backed worker.
    pub fn llama(models: Vec<String>, base_url: impl Into<String>) -> Self {
        Self::new("llama.cpp", models, base_url)
    }

    /// Probe the engine for the models it currently has loaded (`GET {base}/v1/models`). Best-effort:
    /// returns an empty vec if the engine is down or doesn't implement it.
    pub async fn discover_models(&self) -> Vec<String> {
        let url = format!("{}/v1/models", self.base_url);
        let Ok(resp) = self.http.get(&url).send().await else { return Vec::new() };
        let Ok(v) = resp.json::<serde_json::Value>().await else { return Vec::new() };
        v["data"]
            .as_array()
            .map(|a| a.iter().filter_map(|m| m["id"].as_str().map(String::from)).collect())
            .unwrap_or_default()
    }

    pub async fn infer(&self, req: &InferRequest) -> Result<InferResponse> {
        let messages: Vec<serde_json::Value> = req
            .messages
            .iter()
            .map(|m| serde_json::json!({ "role": m.role, "content": m.content }))
            .collect();
        let mut body = serde_json::json!({
            "model": req.model,
            "messages": messages,
            "max_tokens": req.params.max_tokens,
            "temperature": req.params.temperature,
            "stream": false,
        });
        if let Some(top_p) = req.params.top_p {
            body["top_p"] = serde_json::json!(top_p);
        }
        if !req.params.stop.is_empty() {
            body["stop"] = serde_json::json!(req.params.stop);
        }

        let url = format!("{}/v1/chat/completions", self.base_url);
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("calling {} engine at {url}", self.label))?;
        let status = resp.status();
        let v: serde_json::Value = resp.json().await.context("decoding engine response")?;
        if !status.is_success() {
            return Err(anyhow!("{} engine {status}: {v}", self.label));
        }

        let text = v["choices"][0]["message"]["content"].as_str().unwrap_or_default().to_string();
        let finish = v["choices"][0]["finish_reason"].as_str().unwrap_or("stop").to_string();
        let prompt_tokens = v["usage"]["prompt_tokens"].as_u64().unwrap_or(0) as u32;
        let completion_tokens =
            v["usage"]["completion_tokens"].as_u64().unwrap_or_else(|| word_count(&text) as u64) as u32;

        Ok(InferResponse {
            request_id: req.request_id.clone(),
            model: req.model.clone(),
            text,
            prompt_tokens,
            completion_tokens,
            finish_reason: finish,
            error: None,
        })
    }
}

fn word_count(s: &str) -> u32 {
    s.split_whitespace().count() as u32
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ce_exo_core::proto::{ChatMessage, SampleParams};

    fn req(model: &str, user: &str, max: u32) -> InferRequest {
        InferRequest {
            request_id: "r1".into(),
            model: model.into(),
            messages: vec![ChatMessage { role: "user".into(), content: user.into() }],
            params: SampleParams { max_tokens: max, ..Default::default() },
            stream: false,
            cap_chain_hex: String::new(),
        }
    }

    #[test]
    fn mock_is_deterministic_and_bounded() {
        let b = MockBackend::new(vec!["mock-tiny".into()]);
        let r1 = b.infer(&req("mock-tiny", "hello world", 8));
        let r2 = b.infer(&req("mock-tiny", "hello world", 8));
        assert_eq!(r1.text, r2.text);
        assert!(r1.completion_tokens <= 8);
        assert_eq!(r1.finish_reason, "stop");
    }

    #[test]
    fn engine_labels_and_serves() {
        let b = Backend::Engine(EngineBackend::exo(vec!["llama-3.1-8b".into()], EXO_DEFAULT_URL));
        assert_eq!(b.name(), "exo");
        assert!(b.serves("llama-3.1-8b"));
        assert!(!b.serves("nope"));
    }
}

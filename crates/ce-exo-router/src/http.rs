//! HTTP front door — OpenAI- and Ollama-compatible endpoints over the router's mesh dispatch.
//!
//! Supported:
//! - `POST /v1/chat/completions` (OpenAI; streaming + non-streaming)
//! - `GET  /v1/models`           (OpenAI)
//! - `POST /api/chat`            (Ollama; streaming NDJSON + non-streaming)
//! - `POST /api/generate`        (Ollama)
//! - `GET  /api/tags`            (Ollama model list)
//! - `GET  /exo/fleet`           (ce-exo fleet view)
//! - `GET  /healthz`
//!
//! Streaming is synthesized from the completed mesh response (chunked back to the client). Native
//! token-by-token passthrough from the worker is a planned refinement.

use crate::Router;
use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json,
};
use ce_exo_core::proto::{ChatMessage, InferResponse, SampleParams};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

/// Build the axum app exposing the cluster's public HTTP API.
pub fn build_app(router: Arc<Router>) -> axum::Router {
    axum::Router::new()
        .route("/", get(ui))
        .route("/ui", get(ui))
        .route("/healthz", get(healthz))
        .route("/v1/models", get(openai_models))
        .route("/v1/chat/completions", post(openai_chat))
        .route("/api/chat", post(ollama_chat))
        .route("/api/generate", post(ollama_generate))
        .route("/api/tags", get(ollama_tags))
        .route("/exo/fleet", get(exo_fleet))
        .with_state(router)
}

// ---------- shared helpers ----------

fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn params_from(
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    stop: Option<Vec<String>>,
) -> SampleParams {
    let d = SampleParams::default();
    SampleParams {
        max_tokens: max_tokens.unwrap_or(d.max_tokens),
        temperature: temperature.unwrap_or(d.temperature),
        top_p,
        stop: stop.unwrap_or_default(),
    }
}

/// Split text into streaming pieces on whitespace boundaries (keeps the spaces).
fn pieces(text: &str) -> Vec<String> {
    text.split_inclusive(char::is_whitespace).map(|s| s.to_string()).collect()
}

fn error_response(code: StatusCode, msg: impl Into<String>) -> Response {
    let msg = msg.into();
    (code, Json(json!({ "error": { "message": msg, "type": "ce_exo_error" } }))).into_response()
}

fn sse(body: String) -> Response {
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

fn ndjson(body: String) -> Response {
    ([(header::CONTENT_TYPE, "application/x-ndjson")], body).into_response()
}

// ---------- health + fleet ----------

async fn healthz() -> &'static str {
    "ok"
}

/// The built-in chat web UI (one public domain serves API + UI).
async fn ui() -> Html<&'static str> {
    Html(include_str!("../../../ui/index.html"))
}

async fn exo_fleet(State(router): State<Arc<Router>>) -> Response {
    match router.fleet().await {
        Ok(fleet) => Json(json!({ "workers": fleet })).into_response(),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ---------- OpenAI ----------

#[derive(Deserialize)]
struct OpenAiChatReq {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    top_p: Option<f32>,
    #[serde(default)]
    stop: Option<Vec<String>>,
}

async fn openai_models(State(router): State<Arc<Router>>) -> Response {
    let created = unix_secs();
    let data: Vec<Value> = router
        .registry()
        .ids()
        .iter()
        .map(|id| json!({ "id": id, "object": "model", "created": created, "owned_by": "ce-exo" }))
        .collect();
    Json(json!({ "object": "list", "data": data })).into_response()
}

async fn openai_chat(State(router): State<Arc<Router>>, Json(req): Json<OpenAiChatReq>) -> Response {
    let params = params_from(req.max_tokens, req.temperature, req.top_p, req.stop);
    let resp = match router.chat(&req.model, req.messages, params).await {
        Ok(r) => r,
        Err(e) => return error_response(StatusCode::BAD_GATEWAY, e.to_string()),
    };
    let created = unix_secs();
    let id = format!("chatcmpl-{}", resp.request_id);
    if req.stream {
        sse(openai_sse(&id, created, &resp))
    } else {
        Json(openai_completion(&id, created, &resp)).into_response()
    }
}

fn openai_completion(id: &str, created: u64, r: &InferResponse) -> Value {
    json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": r.model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": r.text },
            "finish_reason": r.finish_reason,
        }],
        "usage": {
            "prompt_tokens": r.prompt_tokens,
            "completion_tokens": r.completion_tokens,
            "total_tokens": r.prompt_tokens + r.completion_tokens,
        }
    })
}

fn openai_sse(id: &str, created: u64, r: &InferResponse) -> String {
    let head = |delta: Value, finish: Value| {
        format!(
            "data: {}\n\n",
            json!({
                "id": id, "object": "chat.completion.chunk", "created": created, "model": r.model,
                "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
            })
        )
    };
    let mut out = String::new();
    out.push_str(&head(json!({ "role": "assistant" }), Value::Null));
    for p in pieces(&r.text) {
        out.push_str(&head(json!({ "content": p }), Value::Null));
    }
    out.push_str(&head(json!({}), json!(r.finish_reason)));
    out.push_str("data: [DONE]\n\n");
    out
}

// ---------- Ollama ----------

#[derive(Deserialize)]
struct OllamaChatReq {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(default = "default_true")]
    stream: bool,
    #[serde(default)]
    options: Option<OllamaOptions>,
}

#[derive(Deserialize)]
struct OllamaGenerateReq {
    model: String,
    prompt: String,
    #[serde(default = "default_true")]
    stream: bool,
    #[serde(default)]
    options: Option<OllamaOptions>,
}

#[derive(Deserialize, Default)]
struct OllamaOptions {
    #[serde(default)]
    num_predict: Option<u32>,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    top_p: Option<f32>,
}

fn default_true() -> bool {
    true
}

fn ollama_params(o: &Option<OllamaOptions>) -> SampleParams {
    let o = o.as_ref();
    params_from(
        o.and_then(|x| x.num_predict),
        o.and_then(|x| x.temperature),
        o.and_then(|x| x.top_p),
        None,
    )
}

async fn ollama_chat(State(router): State<Arc<Router>>, Json(req): Json<OllamaChatReq>) -> Response {
    let params = ollama_params(&req.options);
    let resp = match router.chat(&req.model, req.messages, params).await {
        Ok(r) => r,
        Err(e) => return error_response(StatusCode::BAD_GATEWAY, e.to_string()),
    };
    if req.stream {
        ndjson(ollama_chat_ndjson(&resp))
    } else {
        Json(ollama_chat_final(&resp)).into_response()
    }
}

async fn ollama_generate(
    State(router): State<Arc<Router>>,
    Json(req): Json<OllamaGenerateReq>,
) -> Response {
    let params = ollama_params(&req.options);
    let messages = vec![ChatMessage { role: "user".into(), content: req.prompt }];
    let resp = match router.chat(&req.model, messages, params).await {
        Ok(r) => r,
        Err(e) => return error_response(StatusCode::BAD_GATEWAY, e.to_string()),
    };
    if req.stream {
        ndjson(ollama_generate_ndjson(&resp))
    } else {
        Json(json!({
            "model": resp.model, "created_at": iso_secs(unix_secs()),
            "response": resp.text, "done": true,
            "prompt_eval_count": resp.prompt_tokens, "eval_count": resp.completion_tokens,
        }))
        .into_response()
    }
}

fn ollama_chat_final(r: &InferResponse) -> Value {
    json!({
        "model": r.model, "created_at": iso_secs(unix_secs()),
        "message": { "role": "assistant", "content": r.text }, "done": true,
        "prompt_eval_count": r.prompt_tokens, "eval_count": r.completion_tokens,
    })
}

fn ollama_chat_ndjson(r: &InferResponse) -> String {
    let created = iso_secs(unix_secs());
    let mut out = String::new();
    for p in pieces(&r.text) {
        out.push_str(&format!(
            "{}\n",
            json!({ "model": r.model, "created_at": created,
                    "message": { "role": "assistant", "content": p }, "done": false })
        ));
    }
    out.push_str(&format!(
        "{}\n",
        json!({ "model": r.model, "created_at": created,
                "message": { "role": "assistant", "content": "" }, "done": true,
                "prompt_eval_count": r.prompt_tokens, "eval_count": r.completion_tokens })
    ));
    out
}

fn ollama_generate_ndjson(r: &InferResponse) -> String {
    let created = iso_secs(unix_secs());
    let mut out = String::new();
    for p in pieces(&r.text) {
        out.push_str(&format!(
            "{}\n",
            json!({ "model": r.model, "created_at": created, "response": p, "done": false })
        ));
    }
    out.push_str(&format!(
        "{}\n",
        json!({ "model": r.model, "created_at": created, "response": "", "done": true,
                "prompt_eval_count": r.prompt_tokens, "eval_count": r.completion_tokens })
    ));
    out
}

async fn ollama_tags(State(router): State<Arc<Router>>) -> Response {
    let models: Vec<Value> = router
        .registry()
        .models
        .iter()
        .map(|m| {
            json!({
                "name": m.id, "model": m.id,
                "modified_at": iso_secs(0),
                "size": m.weight_mb.saturating_mul(1_048_576),
                "details": { "family": m.family, "parameter_size": format!("{}B", m.params_b), "quantization_level": m.quant },
            })
        })
        .collect();
    Json(json!({ "models": models })).into_response()
}

/// Minimal RFC3339-ish timestamp from unix seconds, no chrono dependency. Clients that parse it only
/// need a well-formed string; ce-exo does not rely on the value.
fn iso_secs(secs: u64) -> String {
    // Good enough: encode the epoch second; most Ollama clients display it verbatim.
    format!("1970-01-01T00:00:{:02}Z+{}", secs % 60, secs)
}

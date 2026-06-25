//! `ce-exo-worker` — run an inference worker that wraps a real engine and joins the ce-exo fleet.

use anyhow::{bail, Context, Result};
use ce_exo_core::HardwareProbe;
use ce_exo_worker::{
    parse_node_id, Backend, EngineBackend, EngineProcess, MockBackend, Worker, WorkerConfig,
    EXO_DEFAULT_URL, LLAMA_DEFAULT_URL,
};
use ce_identity::NodeId;
use ce_rs::CeClient;
use clap::Parser;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "ce-exo-worker", version, about = "ce-exo inference worker — a CE mesh service wrapping a real engine")]
struct Args {
    /// Engine to wrap: `exo` (default), `llama`, `openai` (any OpenAI-compatible URL), or `mock`.
    #[arg(long, default_value = "exo")]
    backend: String,

    /// A model id this worker serves (repeatable). If omitted for an engine backend, the worker
    /// queries the engine's `/v1/models`.
    #[arg(long = "model", value_name = "ID")]
    models: Vec<String>,

    /// The engine's OpenAI-compatible base URL. Defaults: exo -> :52415, llama -> :8081.
    #[arg(long, env = "CE_EXO_ENGINE_URL")]
    engine_url: Option<String>,

    /// Launch and supervise the engine with this shell command (kept alive while the worker runs).
    /// e.g. `exo --chatgpt-api-port 52415`  or  `docker run --rm --gpus all --network host exo-image`.
    #[arg(long)]
    engine_cmd: Option<String>,

    /// Dev only: skip capability enforcement (single-user / self-host). Never use on a shared host.
    #[arg(long, env = "CE_EXO_OPEN")]
    open: bool,

    /// Local CE node HTTP API base URL.
    #[arg(long, default_value = ce_rs::DEFAULT_BASE_URL, env = "CE_API_URL")]
    node_url: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    let args = Args::parse();

    let probe = HardwareProbe::detect();
    let engine_url = args
        .engine_url
        .clone()
        .unwrap_or_else(|| match args.backend.as_str() {
            "llama" => LLAMA_DEFAULT_URL.to_string(),
            _ => EXO_DEFAULT_URL.to_string(),
        });

    // Optionally launch the engine and wait for it to come up.
    let mut engine_proc = match &args.engine_cmd {
        Some(cmd) => {
            let p = EngineProcess::spawn(cmd)?;
            p.wait_ready(&engine_url, Duration::from_secs(180)).await?;
            Some(p)
        }
        None => None,
    };

    let backend = build_backend(&args.backend, args.models.clone(), &engine_url).await?;
    if backend.models_ready().is_empty() {
        if let Some(mut p) = engine_proc.take() {
            p.stop().await;
        }
        bail!(
            "no models available: pass --model <id>, or ensure the engine at {engine_url} reports models on /v1/models"
        );
    }

    let ce = CeClient::new(args.node_url.clone());
    if let Err(e) = ce.health().await.context("local CE node not reachable — is `ce start` running?") {
        if let Some(mut p) = engine_proc.take() {
            p.stop().await;
        }
        return Err(e);
    }

    let cfg = WorkerConfig { open: args.open, accepted_roots: load_roots() };
    if cfg.open {
        tracing::warn!("running with --open: capability enforcement is DISABLED (dev mode)");
    }
    let worker = Worker::new(ce, backend, probe, cfg).await?;
    let res = ce_exo_worker::run(worker).await;

    if let Some(mut p) = engine_proc.take() {
        p.stop().await;
    }
    res
}

/// Build a backend, discovering models from the engine when none were given.
async fn build_backend(kind: &str, models: Vec<String>, engine_url: &str) -> Result<Backend> {
    match kind {
        "mock" => {
            let models = if models.is_empty() { vec!["mock-tiny".to_string()] } else { models };
            Ok(Backend::Mock(MockBackend::new(models)))
        }
        "exo" | "llama" | "openai" => {
            let label = if kind == "llama" { "llama.cpp" } else { kind };
            let mut eb = EngineBackend::new(label, models.clone(), engine_url);
            if models.is_empty() {
                let discovered = eb.discover_models().await;
                if !discovered.is_empty() {
                    tracing::info!(?discovered, "discovered models from engine");
                    eb = EngineBackend::new(label, discovered, engine_url);
                }
            }
            Ok(Backend::Engine(eb))
        }
        other => bail!("unknown backend '{other}' (expected exo | llama | openai | mock)"),
    }
}

/// Trust roots from `CE_EXO_ROOTS` (comma-separated 64-hex NodeIds).
fn load_roots() -> Vec<NodeId> {
    std::env::var("CE_EXO_ROOTS")
        .ok()
        .map(|s| s.split(',').filter_map(|x| parse_node_id(x.trim()).ok()).collect())
        .unwrap_or_default()
}

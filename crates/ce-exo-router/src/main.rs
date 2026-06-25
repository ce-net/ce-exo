//! `ce-exo-router` — the cluster's public OpenAI/Ollama HTTP front door.

use anyhow::{Context, Result};
use ce_exo_core::registry::Registry;
use ce_exo_router::{http::build_app, Router};
use ce_rs::CeClient;
use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "ce-exo-router", version, about = "ce-exo OpenAI/Ollama-compatible router")]
struct Args {
    /// Address to bind the HTTP front door.
    #[arg(long, default_value = "127.0.0.1:8088", env = "CE_EXO_BIND")]
    bind: String,

    /// Path to the model registry (`models.toml`).
    #[arg(long, default_value = "models.toml", env = "CE_EXO_MODELS")]
    models: PathBuf,

    /// Hex `ce-cap` chain to present to workers (empty = rely on workers running `--open`).
    #[arg(long, default_value = "", env = "CE_EXO_GRANT")]
    grant: String,

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

    let registry = if args.models.exists() {
        Registry::load(&args.models)?
    } else {
        tracing::warn!(path = %args.models.display(), "no model registry found; /v1/models will be empty");
        Registry::default()
    };

    let ce = CeClient::new(args.node_url);
    ce.health().await.context("local CE node is not reachable — is `ce start` running?")?;

    let router = Router::new(ce, registry, args.grant);
    let app = build_app(router);

    let addr: SocketAddr = args.bind.parse().with_context(|| format!("invalid --bind '{}'", args.bind))?;
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, "ce-exo router listening (OpenAI: /v1, Ollama: /api)");
    axum::serve(listener, app).await.context("serving HTTP")?;
    Ok(())
}

//! `ce-exo` — the umbrella CLI for running LLMs distributed across a CE mesh, wrapping a real engine
//! (exo by default; also llama.cpp / any OpenAI-compatible server).
//!
//!   ce-exo serve   --backend exo --launch --engine-cmd 'exo --chatgpt-api-port 52415' --open
//!   ce-exo cluster --member <node:port> --member <node:port>   # stitch exo across machines via CE
//!   ce-exo router  --models models.toml                        # one public OpenAI/Ollama front door
//!   ce-exo chat    llama-3.1-8b "hello"                         # talk to the cluster
//!   ce-exo fleet                                               # who's online

use anyhow::{bail, Context, Result};
use ce_exo_core::plan::{Mode, PlanOpts};
use ce_exo_core::registry::Registry;
use ce_exo_core::HardwareProbe;
use ce_exo_router::cluster::{self, ExoMember};
use ce_exo_router::Router;
use ce_exo_sdk::{msg, ExoClient, SampleParams};
use ce_exo_worker::{
    Backend, EngineBackend, EngineProcess, MockBackend, Worker, WorkerConfig, EXO_DEFAULT_URL,
    LLAMA_DEFAULT_URL,
};
use ce_rs::CeClient;
use clap::{Parser, Subcommand};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(
    name = "ce-exo",
    version,
    about = "Run LLMs distributed across your CE mesh — exo (or llama.cpp) wrapped as a CE app."
)]
struct Cli {
    /// Local CE node HTTP API base URL.
    #[arg(long, default_value = ce_rs::DEFAULT_BASE_URL, env = "CE_API_URL", global = true)]
    node_url: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run an inference worker on this machine (wraps a real engine, joins the fleet).
    Serve {
        #[arg(long, default_value = "exo")]
        backend: String,
        #[arg(long = "model", value_name = "ID")]
        models: Vec<String>,
        #[arg(long)]
        engine_url: Option<String>,
        /// Launch + supervise the engine with this shell command while the worker runs.
        #[arg(long)]
        engine_cmd: Option<String>,
        /// Dev only: disable capability enforcement.
        #[arg(long)]
        open: bool,
    },
    /// Deploy a model across the fleet from THIS machine — one command, no per-machine setup.
    /// Launches the worker on each target host over the mesh (rdev `run`, gated by the `spawn`
    /// capability). One-time setup per host: it runs its CE node + `rdev serve` and grants you a cap.
    Deploy {
        model: String,
        /// Target node id (repeatable). If omitted, the best nodes are picked from the atlas.
        #[arg(long = "node", value_name = "NODE_ID")]
        nodes: Vec<String>,
        /// Number of nodes to auto-select when none are named.
        #[arg(long, default_value_t = 2)]
        count: usize,
        /// Worker backend to run on each host (exo default; mock for tests).
        #[arg(long, default_value = "exo")]
        backend: String,
        /// Hex ce-cap token granting the `spawn` ability on the targets (required unless they run --open).
        #[arg(long, default_value = "")]
        grant: String,
        /// Engine launch command run on each host (e.g. 'exo --chatgpt-api-port 52415').
        #[arg(long)]
        engine_cmd: Option<String>,
        /// Engine OpenAI-compatible URL on each host.
        #[arg(long)]
        engine_url: Option<String>,
        /// ce-exo executable name/path on the hosts.
        #[arg(long, default_value = "ce-exo")]
        exe: String,
        /// Pass --open to the deployed workers (dev only).
        #[arg(long)]
        open: bool,
    },
    /// Stitch an exo cluster across machines: open CE tunnels to each member's exo peer port.
    Cluster {
        /// A member as `<node_id_hex>:<exo_peer_port>` (repeatable). Include all members.
        #[arg(long = "member", value_name = "NODE:PORT")]
        members: Vec<String>,
        /// This machine's node id. Defaults to the local node's id.
        #[arg(long)]
        self_node: Option<String>,
        /// First local port to allocate for tunnels.
        #[arg(long, default_value_t = 6000)]
        base_port: u16,
        /// Hex ce-cap token authorizing the tunnels (needs the `tunnel` ability on each member).
        #[arg(long)]
        grant: Option<String>,
    },
    /// Run the OpenAI/Ollama HTTP router — the cluster's one public front door.
    Router {
        #[arg(long, default_value = "127.0.0.1:8088")]
        bind: String,
        #[arg(long, default_value = "models.toml")]
        models: PathBuf,
        #[arg(long, default_value = "")]
        grant: String,
        /// Pin worker node ids (repeatable); skips DHT discovery. Use for static fleets or when the
        /// router and workers are on different nodes you already know.
        #[arg(long = "worker", value_name = "NODE_ID")]
        workers: Vec<String>,
    },
    /// Chat with a model through a router (streams by default).
    Chat {
        model: String,
        prompt: Option<String>,
        #[arg(long, default_value = "http://127.0.0.1:8088")]
        url: String,
        #[arg(long)]
        no_stream: bool,
    },
    /// List the models a router advertises.
    Models {
        #[arg(long, default_value = "http://127.0.0.1:8088")]
        url: String,
    },
    /// Show the live worker fleet (discovered over the mesh).
    Fleet,
    /// Plan how a model would be placed across the fleet (replica vs pipeline-split).
    Up {
        model: String,
        #[arg(long, default_value = "models.toml")]
        models: PathBuf,
        #[arg(long, default_value_t = 1)]
        replicas: usize,
        #[arg(long)]
        force_pipeline: bool,
    },
    /// Show local CE node status.
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve { backend, models, engine_url, engine_cmd, open } => {
            serve(&cli.node_url, &backend, models, engine_url, engine_cmd, open).await
        }
        Cmd::Deploy { model, nodes, count, backend, grant, engine_cmd, engine_url, exe, open } => {
            deploy(&cli.node_url, model, nodes, count, backend, grant, engine_cmd, engine_url, exe, open)
                .await
        }
        Cmd::Cluster { members, self_node, base_port, grant } => {
            cluster_connect(&cli.node_url, members, self_node, base_port, grant).await
        }
        Cmd::Router { bind, models, grant, workers } => {
            router(&cli.node_url, &bind, &models, grant, workers).await
        }
        Cmd::Chat { model, prompt, url, no_stream } => chat(&url, &model, prompt, !no_stream).await,
        Cmd::Models { url } => models_cmd(&url).await,
        Cmd::Fleet => fleet(&cli.node_url).await,
        Cmd::Up { model, models, replicas, force_pipeline } => {
            up(&cli.node_url, &model, &models, replicas, force_pipeline).await
        }
        Cmd::Status => status(&cli.node_url).await,
    }
}

async fn serve(
    node_url: &str,
    backend_kind: &str,
    models: Vec<String>,
    engine_url: Option<String>,
    engine_cmd: Option<String>,
    open: bool,
) -> Result<()> {
    let probe = HardwareProbe::detect();
    let engine_url = engine_url.unwrap_or_else(|| match backend_kind {
        "llama" => LLAMA_DEFAULT_URL.to_string(),
        _ => EXO_DEFAULT_URL.to_string(),
    });

    let mut engine_proc = match &engine_cmd {
        Some(cmd) => {
            let p = EngineProcess::spawn(cmd)?;
            p.wait_ready(&engine_url, Duration::from_secs(180)).await?;
            Some(p)
        }
        None => None,
    };

    let backend = match backend_kind {
        "mock" => {
            let models = if models.is_empty() { vec!["mock-tiny".to_string()] } else { models };
            Backend::Mock(MockBackend::new(models))
        }
        "exo" | "llama" | "openai" => {
            let label = if backend_kind == "llama" { "llama.cpp" } else { backend_kind };
            let mut eb = EngineBackend::new(label, models.clone(), &engine_url);
            if models.is_empty() {
                let discovered = eb.discover_models().await;
                if !discovered.is_empty() {
                    eb = EngineBackend::new(label, discovered, &engine_url);
                }
            }
            Backend::Engine(eb)
        }
        other => bail!("unknown backend '{other}' (expected exo | llama | openai | mock)"),
    };
    if backend.models_ready().is_empty() {
        if let Some(mut p) = engine_proc.take() {
            p.stop().await;
        }
        bail!("no models available; pass --model <id> or ensure the engine reports /v1/models");
    }

    let ce = CeClient::new(node_url.to_string());
    ce.health().await.context("local CE node not reachable — is `ce start` running?")?;
    let worker =
        Worker::new(ce, backend, probe, WorkerConfig { open, accepted_roots: Vec::new() }).await?;
    let res = ce_exo_worker::run(worker).await;
    if let Some(mut p) = engine_proc.take() {
        p.stop().await;
    }
    res
}

#[allow(clippy::too_many_arguments)]
async fn deploy(
    node_url: &str,
    model: String,
    nodes: Vec<String>,
    count: usize,
    backend: String,
    grant: String,
    engine_cmd: Option<String>,
    engine_url: Option<String>,
    exe: String,
    open: bool,
) -> Result<()> {
    use ce_exo_router::orchestrate::{deploy_workers, select_nodes, DeploySpec};
    let ce = CeClient::new(node_url.to_string());
    ce.health().await.context("local CE node not reachable")?;

    let targets = if nodes.is_empty() {
        let picked = select_nodes(&ce, count).await?;
        if picked.is_empty() {
            bail!("no nodes found in the atlas; name targets with --node <id>");
        }
        println!("auto-selected {} node(s) from the atlas", picked.len());
        picked
    } else {
        nodes
    };

    let spec = DeploySpec { backend, model: model.clone(), engine_url, engine_cmd, open, caps: grant, cwd: None, exe };
    println!("deploying '{model}' to {} node(s) over the mesh (rdev run)...", targets.len());
    let results = deploy_workers(&ce, &targets, &spec).await;
    let mut ok = 0;
    for r in &results {
        match &r.job_id {
            Ok(job) => {
                ok += 1;
                println!("  {} -> launched (rdev job {})", short(&r.node_id), short(job));
            }
            Err(e) => println!("  {} -> FAILED: {e}", short(&r.node_id)),
        }
    }
    println!("\n{ok}/{} launched. Bring up the public endpoint with: ce-exo router", results.len());
    if ok == 0 {
        bail!("no deploys succeeded");
    }
    Ok(())
}

async fn cluster_connect(
    node_url: &str,
    members: Vec<String>,
    self_node: Option<String>,
    base_port: u16,
    grant: Option<String>,
) -> Result<()> {
    if members.len() < 2 {
        bail!("need at least 2 --member <node:port> entries to form a cluster");
    }
    let parsed: Result<Vec<ExoMember>> = members
        .iter()
        .map(|m| {
            let (node, port) = m
                .rsplit_once(':')
                .ok_or_else(|| anyhow::anyhow!("member must be <node_id>:<port>, got '{m}'"))?;
            Ok(ExoMember { node_id: node.to_string(), peer_port: port.parse().context("bad port")? })
        })
        .collect();
    let members = parsed?;

    let ce = CeClient::new(node_url.to_string());
    let self_node = match self_node {
        Some(s) => s,
        None => ce.status().await.context("local CE node not reachable")?.node_id,
    };

    let wiring = cluster::wire_local(&self_node, &members, base_port, grant.as_deref());
    if wiring.tunnels.is_empty() {
        println!("nothing to do: this node is the only member");
        return Ok(());
    }
    let token = ce_rs::discover_api_token();
    let peers = cluster::connect_local(ce.base_url(), token.as_deref(), &wiring).await?;
    println!("opened {} CE tunnel(s) to peers across the mesh:", wiring.tunnels.len());
    for t in &wiring.tunnels {
        println!("  127.0.0.1:{} -> {}:{}", t.local_port, short(&t.target_node), t.remote_port);
    }
    println!("\nGive exo these manual-discovery peers on THIS machine:");
    println!("  {}", peers.join(", "));
    Ok(())
}

async fn router(
    node_url: &str,
    bind: &str,
    models_path: &PathBuf,
    grant: String,
    workers: Vec<String>,
) -> Result<()> {
    let registry = if models_path.exists() { Registry::load(models_path)? } else { Registry::default() };
    let ce = CeClient::new(node_url.to_string());
    ce.health().await.context("local CE node not reachable")?;
    let router = Router::with_workers(ce, registry, grant, workers);
    println!("ce-exo router on http://{bind}  (OpenAI: /v1, Ollama: /api)");
    ce_exo_router::serve(router, bind).await
}

async fn chat(url: &str, model: &str, prompt: Option<String>, stream: bool) -> Result<()> {
    let exo = ExoClient::new(url);
    let params = SampleParams::default();

    if let Some(p) = prompt {
        emit(&exo, model, vec![msg::user(p)], &params, stream).await?;
        return Ok(());
    }

    println!("ce-exo chat — model '{model}'. Empty line or Ctrl-D to quit.");
    let mut history = Vec::new();
    let stdin = std::io::stdin();
    loop {
        print!("\nyou> ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        history.push(msg::user(line));
        print!("exo> ");
        std::io::stdout().flush().ok();
        let reply = emit(&exo, model, history.clone(), &params, stream).await?;
        history.push(msg::assistant(reply));
    }
    Ok(())
}

async fn emit(
    exo: &ExoClient,
    model: &str,
    messages: Vec<ce_exo_sdk::ChatMessage>,
    params: &SampleParams,
    stream: bool,
) -> Result<String> {
    if stream {
        let text = exo
            .chat_stream(model, messages, params, |tok| {
                print!("{tok}");
                std::io::stdout().flush().ok();
            })
            .await?;
        println!();
        Ok(text)
    } else {
        let text = exo.chat_with(model, messages, params).await?;
        println!("{text}");
        Ok(text)
    }
}

async fn models_cmd(url: &str) -> Result<()> {
    let models = ExoClient::new(url).models().await?;
    if models.is_empty() {
        println!("(router advertises no models)");
    }
    for m in models {
        println!("{m}");
    }
    Ok(())
}

async fn fleet(node_url: &str) -> Result<()> {
    let ce = CeClient::new(node_url.to_string());
    ce.health().await.context("local CE node not reachable")?;
    let router = Router::new(ce, Registry::default(), String::new());
    let fleet = router.fleet().await?;
    if fleet.is_empty() {
        println!("no workers online (none advertising `exo-host`)");
        return Ok(());
    }
    println!("{:<16}  {:<10}  {:>4}  {:>9}  {:>6}  models", "node", "backend", "gpu", "budget_mb", "active");
    for w in fleet {
        println!(
            "{:<16}  {:<10}  {:>4}  {:>9}  {:>6}  {}",
            short(&w.node_id),
            w.backend,
            if w.has_gpu { "yes" } else { "no" },
            w.budget_mb,
            w.active_requests,
            w.models_ready.join(",")
        );
    }
    Ok(())
}

async fn up(
    node_url: &str,
    model: &str,
    models_path: &PathBuf,
    replicas: usize,
    force_pipeline: bool,
) -> Result<()> {
    let registry = Registry::load(models_path)
        .with_context(|| format!("loading model registry {}", models_path.display()))?;
    let ce = CeClient::new(node_url.to_string());
    ce.health().await.context("local CE node not reachable")?;
    let router = Router::new(ce, registry, String::new());
    let placement =
        router.plan_model(model, &PlanOpts { desired_replicas: replicas, force_pipeline }).await?;
    match placement.mode {
        Mode::Replica => {
            println!("model '{model}' fits a single node — REPLICA mode, {} replica(s):", placement.replicas.len());
            for (i, n) in placement.replicas.iter().enumerate() {
                println!("  replica {i}: {}", short(n));
            }
        }
        Mode::Pipeline => {
            println!("model '{model}' is too big for one node — PIPELINE mode, {} stage(s):", placement.stages.len());
            for (i, s) in placement.stages.iter().enumerate() {
                println!(
                    "  stage {i}: {}  layers {}..={} ({} layers)",
                    short(&s.node_id),
                    s.layer_lo,
                    s.layer_hi,
                    s.layer_count()
                );
            }
            println!("\nForm the exo ring across these nodes with: ce-exo cluster --member <node>:<exo_peer_port> ...");
        }
    }
    Ok(())
}

async fn status(node_url: &str) -> Result<()> {
    let ce = CeClient::new(node_url.to_string());
    let s = ce.status().await.context("local CE node not reachable")?;
    println!("node:    {}", s.node_id);
    println!("peer:    {}", s.peer_id);
    println!("economy: {}", s.economy_enabled());
    // Balance is an economy-ceapp concept (not substrate); read it via the economy SDK and only
    // print it on an economy node (a core/--no-economy node returns an error here).
    if let Ok(bal) = ce_economy::EconomyClient::new(ce).balance().await {
        println!("balance: {}", bal.total.credits());
    }
    Ok(())
}

fn short(node_id: &str) -> String {
    node_id.chars().take(12).collect()
}

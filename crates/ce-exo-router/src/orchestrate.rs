//! Seamless, central deployment — bring a model up across the fleet from **one** machine.
//!
//! The point of CE is that you don't log into each machine. The one-time setup is per host and
//! unavoidable (consent): a machine runs its CE node + `rdev serve` and grants you a capability with
//! the `spawn` (and, for clustering, `tunnel`) ability. **After that, `ce-exo deploy` launches the
//! worker on every target from here, with one command.**
//!
//! ## The host-launch primitive (one dependency, repointable)
//!
//! A CE `mesh-deploy` cell runs sandboxed with `network_mode = "none"` — correct for untrusted
//! marketplace compute, wrong for a long-lived worker that must reach its local node, open tunnels,
//! use the GPU, and stream to peers. So a worker is launched as a **detached host job** through CE's
//! host-exec primitive: a `<ns>/run/start {caps, cmd, cwd}` mesh request, gated by the `spawn`
//! ability, answered with `{ok, job_id}`. ce-exo speaks that protocol directly via `ce-rs` `request`.
//!
//! The CE workspace has several apps in this space (the protocol below was first shipped by `rdev`,
//! which is what is currently installed and what ce-exo's deploy E2E exercises). ce-exo does **not**
//! hardcode any app: the topic namespace is the single constant [`host_launch_ns`] (default `rdev`,
//! overridable with `CE_EXO_LAUNCH_NS`). If/when the canonical host-exec app settles under a new
//! name, repoint here — one line — and nothing else changes.

use anyhow::{anyhow, Result};
use ce_rs::CeClient;
use serde::{Deserialize, Serialize};

/// The mesh topic namespace of the host-exec primitive ce-exo launches workers through. Defaults to
/// `rdev` (the installed, E2E-proven implementation); override with `CE_EXO_LAUNCH_NS` to repoint at
/// the canonical app without a code change.
pub fn host_launch_ns() -> String {
    std::env::var("CE_EXO_LAUNCH_NS").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "rdev".to_string())
}

/// The `run/start` request (extra fields filled by the server from serde defaults).
#[derive(Serialize)]
struct HostLaunchReq {
    caps: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cmd: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
}

/// The subset of the reply we read.
#[derive(Deserialize, Default)]
struct HostLaunchResp {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    job_id: Option<String>,
}

/// What to launch on each target and how to authorize it.
#[derive(Debug, Clone)]
pub struct DeploySpec {
    /// Worker backend to run on the host (`exo` default; `mock` for tests; `llama`/`openai`).
    pub backend: String,
    /// Model id the deployed worker should serve.
    pub model: String,
    /// The engine's OpenAI-compatible base URL on the host (passed to the worker).
    pub engine_url: Option<String>,
    /// Shell command the worker uses to launch + supervise the engine on the host (e.g. `exo …`).
    pub engine_cmd: Option<String>,
    /// Dev only: pass `--open` (skip capability enforcement) to the deployed worker.
    pub open: bool,
    /// Hex `ce-cap` token granting the `spawn` ability on the targets (rdev gates `run` on it).
    pub caps: String,
    /// Working directory on the host (confined to home by rdev).
    pub cwd: Option<String>,
    /// The `ce-exo` executable name/path on the host.
    pub exe: String,
}

impl Default for DeploySpec {
    fn default() -> Self {
        DeploySpec {
            backend: "exo".to_string(),
            model: String::new(),
            engine_url: None,
            engine_cmd: None,
            open: false,
            caps: String::new(),
            cwd: None,
            exe: "ce-exo".to_string(),
        }
    }
}

impl DeploySpec {
    /// The host command that runs the worker (wrapping + optionally launching the engine).
    pub fn worker_command(&self) -> Vec<String> {
        let mut c = vec![
            self.exe.clone(),
            "serve".to_string(),
            "--backend".into(),
            self.backend.clone(),
            "--model".into(),
            self.model.clone(),
        ];
        if let Some(u) = &self.engine_url {
            c.push("--engine-url".into());
            c.push(u.clone());
        }
        if let Some(e) = &self.engine_cmd {
            c.push("--engine-cmd".into());
            c.push(e.clone());
        }
        if self.open {
            c.push("--open".into());
        }
        c
    }
}

/// The outcome of one node's deploy.
#[derive(Debug)]
pub struct DeployResult {
    pub node_id: String,
    /// The rdev host-job id, or the error if the deploy was refused/unreachable.
    pub job_id: Result<String>,
}

/// Launch the worker on `node` via the host-exec primitive's `run/start`, returning the host job id.
async fn deploy_one(ce: &CeClient, node: &str, spec: &DeploySpec) -> Result<String> {
    let req = HostLaunchReq {
        caps: spec.caps.clone(),
        cmd: Some(spec.worker_command()),
        cwd: spec.cwd.clone(),
    };
    let ns = host_launch_ns();
    let topic = format!("{ns}/run/start");
    let payload = serde_json::to_vec(&req)?;
    let bytes = ce
        .request(node, &topic, &payload, 60_000)
        .await
        .map_err(|e| anyhow!("{e} (is the host-exec service '{ns}' running on the target with the `spawn` capability granted?)"))?;
    let r: HostLaunchResp = serde_json::from_slice(&bytes)?;
    if !r.ok {
        return Err(anyhow!("deploy refused: {}", r.error.unwrap_or_else(|| "unknown".into())));
    }
    r.job_id.ok_or_else(|| anyhow!("host did not return a job id"))
}

/// Deploy the worker to every node from this machine. Continues past failures so one unreachable
/// node doesn't abort the rest; inspect each result.
pub async fn deploy_workers(ce: &CeClient, nodes: &[String], spec: &DeploySpec) -> Vec<DeployResult> {
    let mut out = Vec::with_capacity(nodes.len());
    for node in nodes {
        out.push(DeployResult { node_id: node.clone(), job_id: deploy_one(ce, node, spec).await });
    }
    out
}

/// Pick the best `count` nodes for a deployment from the live atlas: prefer GPU, then memory, then
/// least-loaded. Used when the caller doesn't name explicit targets.
pub async fn select_nodes(ce: &CeClient, count: usize) -> Result<Vec<String>> {
    let mut atlas = ce.atlas().await?;
    atlas.sort_by(|a, b| {
        b.has_tag("gpu")
            .cmp(&a.has_tag("gpu"))
            .then(b.mem_mb.cmp(&a.mem_mb))
            .then(a.running_jobs.cmp(&b.running_jobs))
    });
    Ok(atlas.into_iter().take(count).map(|e| e.node_id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_command_includes_engine_and_open() {
        let spec = DeploySpec {
            model: "llama-3.1-8b".into(),
            engine_url: Some("http://127.0.0.1:52415".into()),
            engine_cmd: Some("exo --chatgpt-api-port 52415".into()),
            open: true,
            ..Default::default()
        };
        let c = spec.worker_command();
        assert_eq!(c[0], "ce-exo");
        assert!(c.windows(2).any(|w| w == ["--model", "llama-3.1-8b"]));
        assert!(c.windows(2).any(|w| w == ["--engine-cmd", "exo --chatgpt-api-port 52415"]));
        assert!(c.contains(&"--open".to_string()));
    }
}

//! Seamless, central deployment — bring a model up across the fleet from **one** machine.
//!
//! The whole point of CE is that you do not log into each machine. Once a node has joined your mesh
//! and granted you a capability (a one-time consent — you can't run code on someone's machine without
//! it), deploying ce-exo onto it is a single central action, paid for in credits. This module turns a
//! placement into a set of `mesh-deploy` calls — one directed, capability-gated, billed deploy per
//! target node, issued from the coordinator over the mesh. No SSH, no per-machine setup.
//!
//! `ce.mesh_deploy(node, spec, grant)` runs the worker+engine cell on `node` and bills the bid to the
//! coordinator. The same primitive deploys to a machine you own, a peer's donated machine, or one you
//! rent — anywhere on the mesh, with one call.

use anyhow::Result;
use ce_rs::{Amount, BidSpec, CeClient};

/// What to deploy and how to pay for it.
#[derive(Debug, Clone)]
pub struct DeployOpts {
    /// Container image bundling `ce-exo-worker` + the engine (exo). Pulled by the host on deploy.
    pub image: String,
    /// Model id the deployed worker should serve.
    pub model: String,
    pub cpu_cores: u32,
    pub mem_mb: u64,
    pub duration_secs: u64,
    /// Credits committed per node (the deploy bid).
    pub bid_credits: u64,
    /// Hex `ce-cap` token authorizing `deploy` on the targets (None relies on a self-rooted grant).
    pub grant: Option<String>,
    /// Dev only: pass `--open` to the deployed worker.
    pub open: bool,
}

impl DeployOpts {
    /// The container command that launches the worker against the engine in the same cell.
    fn worker_cmd(&self) -> Vec<String> {
        let mut cmd = vec![
            "ce-exo-worker".to_string(),
            "--backend".into(),
            "exo".into(),
            "--model".into(),
            self.model.clone(),
        ];
        if self.open {
            cmd.push("--open".into());
        }
        cmd
    }

    fn bid_spec(&self) -> BidSpec {
        BidSpec {
            image: self.image.clone(),
            cmd: self.worker_cmd(),
            cpu_cores: self.cpu_cores,
            mem_mb: self.mem_mb,
            duration_secs: self.duration_secs,
            bid: Amount::from_credits(self.bid_credits),
        }
    }
}

/// The outcome of one node's deploy.
#[derive(Debug)]
pub struct DeployResult {
    pub node_id: String,
    /// The host-assigned job id, or the error if the deploy was refused/unreachable.
    pub job_id: Result<String>,
}

/// Deploy the worker cell to every node, directed over the mesh and billed to the coordinator.
/// Continues past failures so one unreachable node doesn't abort the rest; inspect each result.
pub async fn deploy_workers(ce: &CeClient, nodes: &[String], opts: &DeployOpts) -> Vec<DeployResult> {
    let spec = opts.bid_spec();
    let mut out = Vec::with_capacity(nodes.len());
    for node in nodes {
        let job_id = ce.mesh_deploy(node, &spec, opts.grant.as_deref()).await;
        out.push(DeployResult { node_id: node.clone(), job_id });
    }
    out
}

/// Pick the best `count` nodes for an exo deployment from the live atlas: prefer GPU, then memory,
/// then least-loaded. Returns node ids. Used when the caller doesn't name explicit targets.
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

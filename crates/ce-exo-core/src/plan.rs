//! The placement planner — ce-exo's headline concept: **you never choose the mode**.
//!
//! Given a model and a live snapshot of fleet capacity, [`plan`] decides:
//!
//! 1. **Does the model fit on one node?** If at least one node's weight budget holds the whole
//!    model, run it as **replicas**: pick the best `desired_replicas` fitting nodes (highest budget
//!    and history first) so requests load-balance across them for throughput.
//! 2. **Otherwise split it.** Build the **minimal memory-weighted pipeline ring**: walk nodes from
//!    most to least capacity, adding stages until their combined budget covers the model (plus a
//!    safety headroom factor), then assign each stage a contiguous layer range *proportional to its
//!    budget* (the EXO ring). Bigger nodes carry more layers.
//!
//! If even the whole fleet can't hold the model, planning fails with a clear shortfall — never a
//! silent truncation.
//!
//! This module is pure: it takes [`NodeCap`] structs (the caller fills them from `ce.atlas()` +
//! `ce_ratio::history::history()`) and returns a [`Placement`]. No I/O, no mesh calls.

use crate::registry::ModelEntry;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

/// Extra capacity a pipeline must reserve beyond raw weights (KV cache, activations, runtime).
/// A ring is only accepted once combined budget ≥ `weight_mb * PIPELINE_HEADROOM`.
pub const PIPELINE_HEADROOM: f64 = 1.15;

/// How ce-exo will run a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Whole-model copies; requests load-balance across them.
    Replica,
    /// One model split into a layer-range ring across nodes.
    Pipeline,
}

/// One node's contribution, distilled from the CE atlas + history for planning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeCap {
    pub node_id: String,
    /// MB of model weights this node will hold resident (its [`crate::HardwareProbe::weight_budget_mb`]).
    pub budget_mb: u64,
    pub cpu_cores: u32,
    pub has_gpu: bool,
    /// A monotonic "proven host" score (e.g. settled jobs hosted); higher wins ties.
    #[serde(default)]
    pub history: u64,
}

impl NodeCap {
    /// Ranking key: GPU first, then budget, then proven history. Higher is better.
    fn rank_key(&self) -> (bool, u64, u64) {
        (self.has_gpu, self.budget_mb, self.history)
    }
}

/// One contiguous pipeline stage: the layers `[layer_lo, layer_hi]` (inclusive) run on `node_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stage {
    pub node_id: String,
    pub layer_lo: u32,
    pub layer_hi: u32,
    /// CE object CID of this stage's weight shard (the layers it owns). `None` until shards are
    /// published; a worker without it falls back to slicing the full GGUF locally.
    #[serde(default)]
    pub weight_shard_cid: Option<String>,
}

impl Stage {
    pub fn layer_count(&self) -> u32 {
        self.layer_hi.saturating_sub(self.layer_lo) + 1
    }
}

/// A complete plan for serving one model across the fleet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    pub model_id: String,
    pub mode: Mode,
    /// Replica mode: the node ids that each hold the whole model. Empty in pipeline mode.
    #[serde(default)]
    pub replicas: Vec<String>,
    /// Pipeline mode: the ordered ring of stages (stage 0 owns layer 0). Empty in replica mode.
    #[serde(default)]
    pub stages: Vec<Stage>,
}

impl Placement {
    /// Node ids that participate, in order.
    pub fn nodes(&self) -> Vec<&str> {
        match self.mode {
            Mode::Replica => self.replicas.iter().map(String::as_str).collect(),
            Mode::Pipeline => self.stages.iter().map(|s| s.node_id.as_str()).collect(),
        }
    }

    /// Pipeline sanity: stages cover `[0, total_layers)` contiguously with no gap or overlap.
    pub fn pipeline_is_contiguous(&self, total_layers: u32) -> bool {
        if self.mode != Mode::Pipeline {
            return true;
        }
        let mut next = 0u32;
        for s in &self.stages {
            if s.layer_lo != next || s.layer_hi < s.layer_lo {
                return false;
            }
            next = s.layer_hi + 1;
        }
        next == total_layers
    }
}

/// Knobs for [`plan`].
#[derive(Debug, Clone)]
pub struct PlanOpts {
    /// How many whole-model replicas to place when the model fits a single node. Clamped to the
    /// number of fitting nodes.
    pub desired_replicas: usize,
    /// Force pipeline mode even if the model would fit a single node (for benchmarking splits).
    pub force_pipeline: bool,
}

impl Default for PlanOpts {
    fn default() -> Self {
        PlanOpts { desired_replicas: 1, force_pipeline: false }
    }
}

/// Choose a [`Placement`] for `model` over the given fleet snapshot. See the module docs.
pub fn plan(model: &ModelEntry, nodes: &[NodeCap], opts: &PlanOpts) -> Result<Placement> {
    if nodes.is_empty() {
        return Err(anyhow!("no candidate nodes for model '{}'", model.id));
    }

    // Best-capacity first.
    let mut ranked: Vec<&NodeCap> = nodes.iter().collect();
    ranked.sort_by(|a, b| b.rank_key().cmp(&a.rank_key()));

    let fitting: Vec<&NodeCap> =
        ranked.iter().copied().filter(|n| model.fits_single(n.budget_mb)).collect();

    if !opts.force_pipeline && !fitting.is_empty() {
        let take = opts.desired_replicas.clamp(1, fitting.len());
        let replicas = fitting.iter().take(take).map(|n| n.node_id.clone()).collect();
        return Ok(Placement {
            model_id: model.id.clone(),
            mode: Mode::Replica,
            replicas,
            stages: Vec::new(),
        });
    }

    // Pipeline: accumulate the biggest nodes until combined budget covers weights + headroom.
    let need_mb = (model.weight_mb as f64 * PIPELINE_HEADROOM).ceil() as u64;
    let total_fleet: u64 = ranked.iter().map(|n| n.budget_mb).sum();
    if total_fleet < need_mb {
        return Err(anyhow!(
            "model '{}' needs ~{} MB (incl. headroom) but the whole fleet only offers {} MB across {} nodes; \
             add nodes or a smaller quant",
            model.id,
            need_mb,
            total_fleet,
            ranked.len()
        ));
    }

    let mut chosen: Vec<&NodeCap> = Vec::new();
    let mut acc = 0u64;
    for n in &ranked {
        if n.budget_mb == 0 {
            continue;
        }
        chosen.push(n);
        acc += n.budget_mb;
        if acc >= need_mb {
            break;
        }
    }
    // A model can't be split into more stages than it has layers.
    if chosen.len() as u32 > model.layers {
        chosen.truncate(model.layers as usize);
    }

    let stages = assign_layers(model.layers, &chosen);
    let placement = Placement {
        model_id: model.id.clone(),
        mode: Mode::Pipeline,
        replicas: Vec::new(),
        stages,
    };
    debug_assert!(placement.pipeline_is_contiguous(model.layers));
    Ok(placement)
}

/// Assign `total_layers` to `chosen` stages, each getting a share proportional to its budget, with
/// every stage guaranteed at least one layer and the layout kept contiguous. Largest-remainder
/// apportionment keeps the split fair and deterministic.
fn assign_layers(total_layers: u32, chosen: &[&NodeCap]) -> Vec<Stage> {
    let n = chosen.len().max(1);
    let total_budget: u64 = chosen.iter().map(|c| c.budget_mb).sum::<u64>().max(1);

    // Ideal (fractional) layer count per stage, then floor + distribute the remainder by largest
    // fractional part. Reserve one layer per stage so none is empty.
    let assignable = total_layers.saturating_sub(n as u32); // one reserved per stage
    let mut base: Vec<u32> = Vec::with_capacity(n);
    let mut fracs: Vec<(usize, f64)> = Vec::with_capacity(n);
    let mut assigned = 0u32;
    for (i, c) in chosen.iter().enumerate() {
        let ideal = assignable as f64 * (c.budget_mb as f64 / total_budget as f64);
        let floor = ideal.floor() as u32;
        base.push(floor + 1); // +1 reserved layer
        assigned += floor;
        fracs.push((i, ideal - floor as f64));
    }
    // Hand out the leftover layers to the largest fractional parts.
    let mut leftover = assignable.saturating_sub(assigned);
    fracs.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut fi = 0;
    while leftover > 0 && !fracs.is_empty() {
        base[fracs[fi % fracs.len()].0] += 1;
        leftover -= 1;
        fi += 1;
    }

    // Walk contiguous ranges.
    let mut stages = Vec::with_capacity(n);
    let mut lo = 0u32;
    for (i, c) in chosen.iter().enumerate() {
        let count = base[i].max(1);
        let hi = (lo + count - 1).min(total_layers - 1);
        stages.push(Stage {
            node_id: c.node_id.clone(),
            layer_lo: lo,
            layer_hi: hi,
            weight_shard_cid: None,
        });
        lo = hi + 1;
    }
    // Push any rounding remainder onto the last stage so coverage is exact.
    if let Some(last) = stages.last_mut() {
        last.layer_hi = total_layers - 1;
    }
    stages
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, layers: u32, weight_mb: u64) -> ModelEntry {
        ModelEntry {
            id: id.into(),
            family: String::new(),
            params_b: 0.0,
            layers,
            quant: String::new(),
            weight_mb,
            context: 8192,
            object_cid: None,
        }
    }

    fn node(id: &str, budget_mb: u64, gpu: bool, history: u64) -> NodeCap {
        NodeCap { node_id: id.into(), budget_mb, cpu_cores: 8, has_gpu: gpu, history }
    }

    #[test]
    fn replica_when_model_fits_single_node() {
        let m = model("small", 32, 5_000);
        let nodes = vec![node("a", 16_000, true, 10), node("b", 8_000, false, 2)];
        let p = plan(&m, &nodes, &PlanOpts::default()).unwrap();
        assert_eq!(p.mode, Mode::Replica);
        assert_eq!(p.replicas, vec!["a"]); // best (GPU, biggest) chosen
    }

    #[test]
    fn replica_places_multiple_when_requested() {
        let m = model("small", 32, 5_000);
        let nodes = vec![node("a", 16_000, true, 10), node("b", 8_000, false, 2)];
        let opts = PlanOpts { desired_replicas: 5, ..Default::default() };
        let p = plan(&m, &nodes, &opts).unwrap();
        // Only 2 nodes fit, so clamp to 2.
        assert_eq!(p.replicas.len(), 2);
    }

    #[test]
    fn pipeline_when_too_big_for_any_node() {
        let m = model("big", 80, 40_000);
        let nodes = vec![
            node("a", 16_000, true, 5),
            node("b", 16_000, true, 5),
            node("c", 16_000, true, 5),
        ];
        let p = plan(&m, &nodes, &PlanOpts::default()).unwrap();
        assert_eq!(p.mode, Mode::Pipeline);
        assert!(p.pipeline_is_contiguous(80), "stages must tile [0,80)");
        let total: u32 = p.stages.iter().map(|s| s.layer_count()).sum();
        assert_eq!(total, 80);
        assert!(p.stages.iter().all(|s| s.layer_count() >= 1));
    }

    #[test]
    fn pipeline_layers_proportional_to_budget() {
        // 50_000 MB * 1.15 headroom = 57_500 MB <= 60_000 MB fleet.
        let m = model("big", 90, 50_000);
        let nodes = vec![
            node("big", 40_000, true, 5),  // ~2/3 of budget
            node("small", 20_000, true, 5), // ~1/3
        ];
        let p = plan(&m, &nodes, &PlanOpts::default()).unwrap();
        assert_eq!(p.mode, Mode::Pipeline);
        assert_eq!(p.stages.len(), 2);
        // Bigger node should own clearly more layers than the smaller.
        assert!(p.stages[0].layer_count() > p.stages[1].layer_count());
        assert!(p.pipeline_is_contiguous(90));
    }

    #[test]
    fn force_pipeline_overrides_fit() {
        let m = model("small", 16, 4_000);
        let nodes = vec![node("a", 16_000, true, 10), node("b", 16_000, true, 10)];
        let opts = PlanOpts { force_pipeline: true, ..Default::default() };
        let p = plan(&m, &nodes, &opts).unwrap();
        assert_eq!(p.mode, Mode::Pipeline);
    }

    #[test]
    fn fails_when_fleet_too_small() {
        let m = model("huge", 80, 200_000);
        let nodes = vec![node("a", 16_000, true, 5), node("b", 16_000, true, 5)];
        let err = plan(&m, &nodes, &PlanOpts::default()).unwrap_err();
        assert!(err.to_string().contains("whole fleet"));
    }

    #[test]
    fn stages_never_exceed_layers() {
        let m = model("tiny-deep", 3, 30_000);
        let nodes: Vec<NodeCap> = (0..10).map(|i| node(&format!("n{i}"), 4_000, false, 0)).collect();
        let p = plan(&m, &nodes, &PlanOpts::default()).unwrap();
        assert!(p.stages.len() as u32 <= 3);
        assert!(p.pipeline_is_contiguous(3));
    }
}

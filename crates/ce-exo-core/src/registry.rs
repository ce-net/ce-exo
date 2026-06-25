//! Model registry — the catalog of models ce-exo can run, loaded from `models.toml`.
//!
//! Each entry records what placement needs: the model's quantized **weight size**, its
//! **layer count** (for pipeline splitting), context window, and the CE **object CID** of its
//! weights (a content-addressed GGUF, fetched + verified by workers over the CE blob store). An
//! entry with no `object_cid` is a known model whose weights have not been published to the mesh yet.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// One catalog model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelEntry {
    /// Stable id used on the wire and the CLI, e.g. `llama3.1-8b-q4`.
    pub id: String,
    /// Human family / base, e.g. `llama3.1`.
    #[serde(default)]
    pub family: String,
    /// Parameter count in billions (display + sanity only).
    #[serde(default)]
    pub params_b: f64,
    /// Number of transformer blocks — the unit of pipeline splitting.
    pub layers: u32,
    /// Quantization label, e.g. `Q4_K_M`.
    #[serde(default)]
    pub quant: String,
    /// On-disk weight size in MB (the figure placement budgets against).
    pub weight_mb: u64,
    /// Context window in tokens.
    #[serde(default = "default_context")]
    pub context: u32,
    /// CE object CID of the full GGUF weights, if published to the mesh.
    #[serde(default)]
    pub object_cid: Option<String>,
}

fn default_context() -> u32 {
    8192
}

impl ModelEntry {
    /// Per-stage weight MB if this model's layers are split evenly across `stages` nodes (ceil).
    /// Embedding/head overhead is folded into the per-layer average — good enough for budgeting.
    pub fn weight_mb_for_stages(&self, stages: u32) -> u64 {
        let stages = stages.max(1) as u64;
        self.weight_mb.div_ceil(stages)
    }

    /// True if this model's full weights fit within `budget_mb` on a single node (replica-viable).
    pub fn fits_single(&self, budget_mb: u64) -> bool {
        self.weight_mb <= budget_mb
    }
}

/// The loaded catalog.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default, rename = "model")]
    pub models: Vec<ModelEntry>,
}

impl Registry {
    /// Parse a registry from `models.toml` text.
    pub fn from_toml(s: &str) -> Result<Self> {
        toml::from_str(s).context("parsing models.toml")
    }

    /// Load a registry from a file path.
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("reading model registry {}", path.display()))?;
        Self::from_toml(&s)
    }

    /// Look up a model by id.
    pub fn get(&self, id: &str) -> Result<&ModelEntry> {
        self.models
            .iter()
            .find(|m| m.id == id)
            .ok_or_else(|| anyhow!("unknown model '{id}' (not in registry)"))
    }

    pub fn ids(&self) -> Vec<&str> {
        self.models.iter().map(|m| m.id.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[[model]]
id = "llama3.1-8b-q4"
family = "llama3.1"
params_b = 8.0
layers = 32
quant = "Q4_K_M"
weight_mb = 4900
context = 8192
object_cid = "abc123"

[[model]]
id = "llama3.1-70b-q4"
family = "llama3.1"
params_b = 70.0
layers = 80
quant = "Q4_K_M"
weight_mb = 40000
"#;

    #[test]
    fn parses_and_looks_up() {
        let r = Registry::from_toml(SAMPLE).unwrap();
        assert_eq!(r.models.len(), 2);
        let m = r.get("llama3.1-8b-q4").unwrap();
        assert_eq!(m.layers, 32);
        assert_eq!(m.context, 8192);
        assert_eq!(m.object_cid.as_deref(), Some("abc123"));
        assert!(r.get("nope").is_err());
    }

    #[test]
    fn fit_and_split_math() {
        let r = Registry::from_toml(SAMPLE).unwrap();
        let big = r.get("llama3.1-70b-q4").unwrap();
        assert!(!big.fits_single(16_000));
        assert!(big.fits_single(48_000));
        // 40000 MB across 4 stages -> 10000 MB each.
        assert_eq!(big.weight_mb_for_stages(4), 10_000);
        assert_eq!(big.context, 8192); // default applied
    }
}

//! Hardware probe — what this node can contribute to a model.
//!
//! Inference capacity is dominated by **memory for weights**: VRAM if there's a usable GPU, else
//! system RAM. The probe reports cores, RAM, and (best-effort) GPU presence + VRAM, then derives a
//! conservative *weight budget* — the bytes of model weights this node is willing to hold resident.
//!
//! Detection is std-only and best-effort (Linux `/proc`, presence files for NVIDIA). Anything the
//! probe cannot read cleanly can be pinned with an env override, so a node operator always has the
//! final say:
//!
//! - `CE_EXO_RAM_MB`   — total system RAM in MB.
//! - `CE_EXO_VRAM_MB`  — usable GPU VRAM in MB (also forces `has_gpu = true`).
//! - `CE_EXO_BUDGET_FRACTION` — fraction (0.0–0.95) of the chosen memory pool to offer for weights.

use serde::{Deserialize, Serialize};

/// Default fraction of the memory pool offered for resident model weights. The remainder covers the
/// KV cache, activations, runtime, and the rest of the OS.
pub const DEFAULT_BUDGET_FRACTION: f64 = 0.70;

/// Whether the node's usable memory pool is its GPU VRAM or its system RAM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemClass {
    /// Weights live in GPU VRAM (fast).
    Vram,
    /// Weights live in system RAM (CPU / unified-memory inference).
    Ram,
}

/// A snapshot of this node's inference-relevant hardware.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HardwareProbe {
    pub os: String,
    pub arch: String,
    pub cpu_cores: u32,
    /// Total system RAM in MB (0 if undetectable and unset).
    pub ram_mb: u64,
    /// Usable GPU VRAM in MB, if a GPU was detected or pinned.
    pub vram_mb: Option<u64>,
    pub has_gpu: bool,
    /// Which pool [`weight_budget_mb`](Self::weight_budget_mb) draws from.
    pub mem_class: MemClass,
    /// Fraction of the pool offered for weights.
    pub budget_fraction: f64,
}

impl HardwareProbe {
    /// Probe the current machine, applying any `CE_EXO_*` env overrides.
    pub fn detect() -> Self {
        let os = std::env::consts::OS.to_string();
        let arch = std::env::consts::ARCH.to_string();
        let cpu_cores = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1);

        let ram_mb = env_u64("CE_EXO_RAM_MB").unwrap_or_else(detect_ram_mb);

        let vram_override = env_u64("CE_EXO_VRAM_MB");
        let has_gpu = vram_override.is_some() || detect_nvidia();
        let vram_mb = vram_override.or(if has_gpu { Some(0) } else { None });

        let mem_class = if has_gpu && vram_mb.unwrap_or(0) > 0 { MemClass::Vram } else { MemClass::Ram };

        let budget_fraction = env_f64("CE_EXO_BUDGET_FRACTION")
            .filter(|f| *f > 0.0 && *f <= 0.95)
            .unwrap_or(DEFAULT_BUDGET_FRACTION);

        HardwareProbe { os, arch, cpu_cores, ram_mb, vram_mb, has_gpu, mem_class, budget_fraction }
    }

    /// The size of the chosen memory pool in MB (VRAM when GPU weights are viable, else RAM).
    pub fn pool_mb(&self) -> u64 {
        match self.mem_class {
            MemClass::Vram => self.vram_mb.unwrap_or(0),
            MemClass::Ram => self.ram_mb,
        }
    }

    /// MB of model weights this node is willing to hold resident — the input to placement.
    pub fn weight_budget_mb(&self) -> u64 {
        (self.pool_mb() as f64 * self.budget_fraction) as u64
    }

    /// CE capability self-tags this node should advertise so the fleet can find it
    /// (`exo-host`, plus `gpu` / OS / arch). The CE node already advertises generic `gpu`/`docker`
    /// tags; ce-exo adds its own service tag.
    pub fn service_tags(&self) -> Vec<String> {
        let mut t = vec!["exo-host".to_string(), self.os.clone(), self.arch.clone()];
        if self.has_gpu {
            t.push("gpu".to_string());
        }
        t
    }
}

fn detect_ram_mb() -> u64 {
    // Linux: parse /proc/meminfo MemTotal (kB). Other OSes: 0 unless CE_EXO_RAM_MB is set.
    if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                let kb: u64 = rest.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(0);
                return kb / 1024;
            }
        }
    }
    0
}

fn detect_nvidia() -> bool {
    std::path::Path::new("/dev/nvidia0").exists()
        || std::path::Path::new("/proc/driver/nvidia/version").exists()
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
}

fn env_f64(key: &str) -> Option<f64> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_is_fraction_of_pool() {
        let p = HardwareProbe {
            os: "linux".into(),
            arch: "x86_64".into(),
            cpu_cores: 8,
            ram_mb: 32_000,
            vram_mb: Some(24_000),
            has_gpu: true,
            mem_class: MemClass::Vram,
            budget_fraction: 0.70,
        };
        assert_eq!(p.pool_mb(), 24_000);
        assert_eq!(p.weight_budget_mb(), 16_800);
    }

    #[test]
    fn ram_class_uses_ram_pool() {
        let p = HardwareProbe {
            os: "macos".into(),
            arch: "aarch64".into(),
            cpu_cores: 10,
            ram_mb: 16_000,
            vram_mb: None,
            has_gpu: false,
            mem_class: MemClass::Ram,
            budget_fraction: 0.50,
        };
        assert_eq!(p.pool_mb(), 16_000);
        assert_eq!(p.weight_budget_mb(), 8_000);
        assert!(p.service_tags().contains(&"exo-host".to_string()));
        assert!(!p.service_tags().contains(&"gpu".to_string()));
    }
}

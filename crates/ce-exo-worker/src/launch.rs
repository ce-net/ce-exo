//! Launching and supervising the engine process (exo / llama-server / a container).
//!
//! ce-exo can run the engine for you: `ce-exo serve --backend exo --launch` spawns the engine, waits
//! for its OpenAI endpoint to come up, joins it to the mesh, and kills it on exit. The launch command
//! is a plain shell string so it works across engine versions and platforms without ce-exo baking in
//! version-specific flags:
//!
//! - exo (native, Apple Silicon):  `exo --chatgpt-api-port 52415`
//! - exo (container, Linux/GPU):   `docker run --rm --gpus all --network host exo-image`
//! - llama.cpp:                    `llama-server -m model.gguf --port 8081 --host 127.0.0.1`
//!
//! On Apple Silicon, exo must run **natively** (Docker has no Metal GPU passthrough); on Linux/NVIDIA
//! a container with `--gpus all` is fine. ce-exo does not decide that for you — you pass the command.

use anyhow::{anyhow, Context, Result};
use std::time::Duration;
use tokio::process::{Child, Command};

/// A supervised engine child process. Dropping it (or calling [`stop`](Self::stop)) kills the engine.
pub struct EngineProcess {
    child: Option<Child>,
    cmd: String,
}

impl EngineProcess {
    /// Spawn `cmd` via the system shell. The child inherits stdout/stderr so its logs are visible.
    pub fn spawn(cmd: &str) -> Result<Self> {
        let mut c = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.arg("/C").arg(cmd);
            c
        } else {
            let mut c = Command::new("sh");
            c.arg("-c").arg(cmd);
            c
        };
        c.kill_on_drop(true);
        let child = c.spawn().with_context(|| format!("spawning engine: {cmd}"))?;
        tracing::info!(cmd, "launched engine process");
        Ok(EngineProcess { child: Some(child), cmd: cmd.to_string() })
    }

    /// Poll `{base}/v1/models` until the engine answers or `timeout` elapses.
    pub async fn wait_ready(&self, base_url: &str, timeout: Duration) -> Result<()> {
        let url = format!("{}/v1/models", base_url.trim_end_matches('/'));
        let http = reqwest::Client::new();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if http.get(&url).send().await.map(|r| r.status().is_success()).unwrap_or(false) {
                tracing::info!(base_url, "engine is ready");
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(anyhow!("engine '{}' did not become ready at {url} within {timeout:?}", self.cmd));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Kill the engine and reap it.
    pub async fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
            tracing::info!("engine process stopped");
        }
    }
}

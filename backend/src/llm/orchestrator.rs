use anyhow::{Context, Result};
use std::env;
use std::sync::Arc;
use tokio::process::Child;
use tokio::process::Command;
use tokio::time::{sleep, Duration};
use tracing::{debug, error, info, warn};

use sysinfo::{System, SystemExt};

pub struct GemmaOrchestrator {
    min_free_gib: f64,
    cmd: String,
    args: Vec<String>,
    host: String,
    port: u16,
    auto_start: bool,
    child: Arc<tokio::sync::Mutex<Option<Child>>>,
}

impl GemmaOrchestrator {
    pub fn from_env() -> Self {
        let min_free_gib = env::var("ROOK_GEMMA_MIN_FREE_GIB")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(6.0);

        let cmd = env::var("ROOK_GEMMA_CMD").unwrap_or_else(|_| "ollama".to_string());
        let raw_args = env::var("ROOK_GEMMA_ARGS").unwrap_or_else(|_| "serve".to_string());
        let args = shell_split::split(&raw_args).unwrap_or_else(|_| vec![raw_args.clone()]);

        let host = env::var("ROOK_GEMMA_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
        let port = env::var("ROOK_GEMMA_PORT")
            .ok()
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(11434);

        let auto_start = env::var("ROOK_GEMMA_AUTO_START")
            .ok()
            .map(|s| s != "0")
            .unwrap_or(true);

        Self {
            min_free_gib,
            cmd,
            args,
            host,
            port,
            auto_start,
            child: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    fn available_gib(&self) -> f64 {
        let mut sys = System::new_all();
        sys.refresh_memory();
        // sysinfo reports KiB
        let avail_kib = sys.available_memory();
        (avail_kib as f64) / 1024.0 / 1024.0
    }

    pub async fn try_start(&self) -> Result<bool> {
        let free = self.available_gib();
        info!(
            "Available memory: {:.2} GiB (need >= {:.2} GiB)",
            free, self.min_free_gib
        );
        if free < self.min_free_gib {
            warn!(
                "Not enough free RAM to start Gemma: {:.2} GiB < {:.2} GiB",
                free, self.min_free_gib
            );
            return Ok(false);
        }

        // Acquire async lock, set child handle, then drop before awaiting
        {
            let mut guard = self.child.lock().await;
            if guard.is_some() {
                debug!("Gemma already running (child handle present)");
                return Ok(true);
            }

            info!("Spawning Gemma command: {} {:?}", self.cmd, self.args);
            let mut command = Command::new(&self.cmd);
            for a in &self.args {
                command.arg(a);
            }
            command.kill_on_drop(true);
            command.stdout(std::process::Stdio::null());
            command.stderr(std::process::Stdio::null());

            crate::os::hide_tokio(&mut command);
            match command.spawn() {
                Ok(child) => {
                    *guard = Some(child);
                }
                Err(e) => {
                    error!("Failed to spawn Gemma process: {}", e);
                    return Err(anyhow::anyhow!(e)).context("spawn failed");
                }
            }
            // guard dropped here at end of scope
        }

        // Wait for service to accept connections (TCP) up to timeout
        let addr = format!("{}:{}", self.host, self.port);
        let mut attempts = 0u8;
        let max_attempts = 12u8; // ~30s with 2.5s sleep
        while attempts < max_attempts {
            attempts += 1;
            match tokio::time::timeout(
                Duration::from_secs(2),
                tokio::net::TcpStream::connect(&addr),
            )
            .await
            {
                Ok(Ok(_stream)) => {
                    info!("Gemma service is responding at {}", addr);
                    return Ok(true);
                }
                Ok(Err(_)) | Err(_) => {
                    debug!(
                        "Gemma not yet ready (attempt {}/{})",
                        attempts, max_attempts
                    );
                    sleep(Duration::from_millis(2500)).await;
                    continue;
                }
            }
        }

        warn!("Gemma did not become ready within timeout; process may be starting in background");
        Ok(true)
    }

    pub async fn stop(&self) -> Result<bool> {
        // Take the child handle out of the mutex, then drop the lock before awaiting kill
        let opt_child = {
            let mut guard = self.child.lock().await;
            guard.take()
        };
        if let Some(mut child) = opt_child {
            info!("Stopping Gemma process");
            match child.kill().await {
                Ok(_) => Ok(true),
                Err(e) => Err(anyhow::anyhow!(e)).context("failed to kill process"),
            }
        } else {
            debug!("No Gemma process to stop");
            Ok(false)
        }
    }

    pub fn is_auto_start(&self) -> bool {
        self.auto_start
    }
}

// Minimal shell-splitting helper; use small crate to parse ROOK_GEMMA_ARGS
// Add dependency by inlining small parser fallback
mod shell_split {
    pub fn split(s: &str) -> Result<Vec<String>, ()> {
        // Very small split: respect simple quoting
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut in_quotes = false;
        for c in s.chars() {
            match c {
                '"' => in_quotes = !in_quotes,
                ' ' if !in_quotes => {
                    if !cur.is_empty() {
                        out.push(cur.clone());
                        cur.clear();
                    }
                }
                ch => cur.push(ch),
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        Ok(out)
    }
}

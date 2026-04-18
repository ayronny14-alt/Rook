#[allow(dead_code)]
use anyhow::Result;
use std::time::Duration;
use tracing::{debug, warn};

pub struct TerminalTool;

impl TerminalTool {
    // Execute a shell command via `cmd /C` and return captured stdout + stderr.
    pub async fn execute(&self, command: &str) -> Result<String> {
        debug!("Executing terminal command: {}", command);

        let cwd = std::env::var("ROOK_TERMINAL_CWD")
            .map(std::path::PathBuf::from)
            .or_else(|_| std::env::current_dir())?;

        let timeout_secs = std::env::var("ROOK_TERMINAL_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30);

        let mut proc = tokio::process::Command::new("cmd");
        crate::os::hide_tokio(&mut proc)
            .args(["/C", command])
            .current_dir(&cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let output = tokio::time::timeout(Duration::from_secs(timeout_secs), proc.output())
            .await
            .map_err(|_| anyhow::anyhow!("Command timed out after {} seconds", timeout_secs))?
            .map_err(|e| anyhow::anyhow!("Failed to spawn command: {}", e))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let mut result = String::new();
        if !stdout.trim().is_empty() {
            result.push_str(stdout.trim());
        }
        if !stderr.trim().is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str("[stderr] ");
            result.push_str(stderr.trim());
        }
        if !output.status.success() {
            warn!("Command '{}' exited with status {}", command, output.status);
            if result.is_empty() {
                result = format!("[exit code {}]", output.status.code().unwrap_or(-1));
            }
        }
        if result.is_empty() {
            result = "[no output]".to_string();
        }

        Ok(result)
    }
}

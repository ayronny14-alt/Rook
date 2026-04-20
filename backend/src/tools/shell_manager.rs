//
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;

/// Maximum output buffer per shell before truncation (in characters).
const MAX_BUFFER_CHARS: usize = 64 * 1024;

/// Default synchronous-command timeout in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 45;

#[derive(Debug)]
pub struct PersistentShell {
    pub name: String,
    /// Per-shell working directory. Starts at process cwd, updated when the
    /// agent runs `cd <path>` (we detect this pattern and apply it in Rust
    /// because `cmd /C` doesn't persist state between invocations).
    pub cwd: std::path::PathBuf,
    /// Environment overrides set by `set_env` commands.
    pub env: HashMap<String, String>,
    /// Running background command, if any.
    pub background: Option<Child>,
    /// Rolling stdout buffer from background commands + last sync command.
    pub output_buffer: String,
    /// Wall-clock timestamp of last command start, for age tracking.
    pub last_used: std::time::Instant,
}

impl PersistentShell {
    fn new(name: &str) -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        Self {
            name: name.to_string(),
            cwd,
            env: HashMap::new(),
            background: None,
            output_buffer: String::new(),
            last_used: std::time::Instant::now(),
        }
    }

    /// Append to the output buffer, trimming from the front if it grows too large.
    fn push_output(&mut self, chunk: &str) {
        self.output_buffer.push_str(chunk);
        if self.output_buffer.len() > MAX_BUFFER_CHARS {
            let drop = self.output_buffer.len() - MAX_BUFFER_CHARS;
            self.output_buffer.drain(..drop);
            self.output_buffer
                .insert_str(0, "[... earlier output truncated ...]\n");
        }
    }
}

#[derive(Clone)]
pub struct ShellManager {
    shells: Arc<Mutex<HashMap<String, PersistentShell>>>,
}

impl ShellManager {
    pub fn new() -> Self {
        Self {
            shells: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Create a new named shell. Returns an error if a shell with that name
    /// already exists (caller should kill it first).
    pub async fn spawn(&self, name: &str) -> Result<String> {
        let mut shells = self.shells.lock().await;
        if shells.contains_key(name) {
            return Err(anyhow!("Shell '{}' already exists", name));
        }
        let shell = PersistentShell::new(name);
        let cwd = shell.cwd.display().to_string();
        shells.insert(name.to_string(), shell);
        Ok(format!("Spawned shell '{}' (cwd={})", name, cwd))
    }

    /// Ensure a shell exists (create if missing). Returns true if a new
    /// shell was created, false if it already existed.
    async fn ensure(&self, name: &str) -> bool {
        let mut shells = self.shells.lock().await;
        if !shells.contains_key(name) {
            shells.insert(name.to_string(), PersistentShell::new(name));
            true
        } else {
            false
        }
    }

    /// Run a command in a specific shell WITH streaming output.
    /// Each stdout/stderr line is forwarded through `on_line` (typically a
    /// channel sender) AND collected into the returned string.
    pub async fn execute_streaming<F>(
        &self,
        shell_name: &str,
        command: &str,
        mut on_line: F,
        timeout_secs: Option<u64>,
    ) -> Result<String>
    where
        F: FnMut(&str),
    {
        // Auto-create the shell + snapshot cwd/env
        let _ = self.ensure(shell_name).await;
        let (cwd, env_vars) = {
            let shells = self.shells.lock().await;
            let shell = shells
                .get(shell_name)
                .ok_or_else(|| anyhow!("Shell '{}' not found", shell_name))?;
            (shell.cwd.clone(), shell.env.clone())
        };

        let (prog, args_vec) = if cfg!(target_os = "windows") {
            ("cmd", vec!["/C", command])
        } else {
            ("sh", vec!["-c", command])
        };
        let mut cmd = Command::new(prog);
        cmd.args(&args_vec)
            .current_dir(&cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in &env_vars {
            cmd.env(k, v);
        }
        crate::os::hide_tokio(&mut cmd);

        let mut child = cmd.spawn().map_err(|e| anyhow!("spawn failed: {}", e))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| anyhow!("no stderr"))?;

        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut out_reader = BufReader::new(stdout).lines();
        let mut err_reader = BufReader::new(stderr).lines();

        let t = timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(t);
        let mut collected = String::new();
        let mut timed_out = false;

        loop {
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => {
                    timed_out = true;
                    let _ = child.kill().await;
                    break;
                }
                line = out_reader.next_line() => match line {
                    Ok(Some(l)) => {
                        on_line(&l);
                        collected.push_str(&l);
                        collected.push('\n');
                    }
                    Ok(None) => {
                        // stdout closed - still drain stderr then finish
                        while let Ok(Some(l)) = err_reader.next_line().await {
                            let msg = format!("[stderr] {}", l);
                            on_line(&msg);
                            collected.push_str(&msg);
                            collected.push('\n');
                        }
                        break;
                    }
                    Err(e) => {
                        let msg = format!("[read error] {}", e);
                        on_line(&msg);
                        collected.push_str(&msg);
                        break;
                    }
                },
                line = err_reader.next_line() => match line {
                    Ok(Some(l)) => {
                        let msg = format!("[stderr] {}", l);
                        on_line(&msg);
                        collected.push_str(&msg);
                        collected.push('\n');
                    }
                    Ok(None) => {}
                    Err(_) => {}
                }
            }
        }

        if timed_out {
            collected.push_str(&format!("\n[timed out after {}s]\n", t));
        } else {
            match child.wait().await {
                Ok(status) => {
                    if !status.success() {
                        collected
                            .push_str(&format!("\n[exit code {}]\n", status.code().unwrap_or(-1)));
                    }
                }
                Err(e) => {
                    collected.push_str(&format!("\n[wait error: {}]\n", e));
                }
            }
        }

        let mut shells = self.shells.lock().await;
        if let Some(shell) = shells.get_mut(shell_name) {
            shell.push_output(&collected);
            shell.last_used = std::time::Instant::now();
        }

        Ok(collected)
    }

    // Run a command in a specific shell.
    pub async fn execute(
        &self,
        shell_name: &str,
        command: &str,
        background: bool,
        timeout_secs: Option<u64>,
    ) -> Result<String> {
        // Auto-create shells on first use so the agent can skip the explicit
        // `shell_spawn` call for quick one-offs.
        let created = self.ensure(shell_name).await;
        let created_note = if created {
            format!("[auto-spawned shell '{}']\n", shell_name)
        } else {
            String::new()
        };

        // Snapshot the cwd + env while holding the lock briefly, then release
        // it so concurrent shells don't serialize on each other.
        let (cwd, env_vars) = {
            let shells = self.shells.lock().await;
            let shell = shells
                .get(shell_name)
                .ok_or_else(|| anyhow!("Shell '{}' not found", shell_name))?;
            (shell.cwd.clone(), shell.env.clone())
        };

        // Intercept `cd <path>` so the working directory actually persists.
        // `cmd /C` would run cd then exit, losing the change.
        let trimmed = command.trim();
        if let Some(rest) = trimmed.strip_prefix("cd ") {
            let target = rest.trim().trim_matches(|c| c == '"' || c == '\'');
            let new_cwd = if std::path::Path::new(target).is_absolute() {
                std::path::PathBuf::from(target)
            } else {
                cwd.join(target)
            };
            let resolved = std::fs::canonicalize(&new_cwd)
                .map_err(|e| anyhow!("cd failed: {}: {}", new_cwd.display(), e))?;
            let display = resolved.display().to_string();
            let mut shells = self.shells.lock().await;
            if let Some(shell) = shells.get_mut(shell_name) {
                shell.cwd = resolved;
                shell.last_used = std::time::Instant::now();
            }
            return Ok(format!(
                "{}[{}] cwd changed to {}",
                created_note, shell_name, display
            ));
        }

        // Also intercept `set KEY=VALUE` (Windows) and `export KEY=VALUE` (unix)
        // so the agent can persist env variables across calls in a shell.
        if let Some(assignment) = trimmed
            .strip_prefix("set ")
            .or_else(|| trimmed.strip_prefix("export "))
        {
            if let Some((k, v)) = assignment.split_once('=') {
                let mut shells = self.shells.lock().await;
                if let Some(shell) = shells.get_mut(shell_name) {
                    shell.env.insert(k.trim().to_string(), v.trim().to_string());
                }
                return Ok(format!(
                    "{}[{}] set {}={}",
                    created_note,
                    shell_name,
                    k.trim(),
                    v.trim()
                ));
            }
        }

        // Build the subprocess
        let (prog, args) = if cfg!(target_os = "windows") {
            ("cmd", vec!["/C", command])
        } else {
            ("sh", vec!["-c", command])
        };
        let mut cmd = Command::new(prog);
        cmd.args(&args)
            .current_dir(&cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in &env_vars {
            cmd.env(k, v);
        }
        crate::os::hide_tokio(&mut cmd);

        if background {
            // Spawn and stash the handle
            let child = cmd
                .spawn()
                .map_err(|e| anyhow!("Failed to spawn background process: {}", e))?;
            let pid = child.id().unwrap_or(0);
            let mut shells = self.shells.lock().await;
            if let Some(shell) = shells.get_mut(shell_name) {
                // Kill any previous background task for this shell
                if let Some(mut prev) = shell.background.take() {
                    let _ = prev.kill().await;
                }
                shell.background = Some(child);
                shell.last_used = std::time::Instant::now();
            }
            return Ok(format!(
                "{}[{}] Started '{}' in background (pid={}). Use shell_read to fetch output later.",
                created_note, shell_name, command, pid
            ));
        }

        // Synchronous with timeout
        let t = timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
        let result = timeout(Duration::from_secs(t), cmd.output()).await;

        let output = match result {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return Err(anyhow!("Failed to run command: {}", e)),
            Err(_) => {
                return Ok(format!(
                    "{}[{}] Command timed out after {}s. Use run_in_background=true for long-running commands.",
                    created_note, shell_name, t
                ));
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let exit = output.status.code().unwrap_or(-1);

        let mut result = String::new();
        if !created_note.is_empty() {
            result.push_str(&created_note);
        }
        result.push_str(&format!("[{}] $ {}\n", shell_name, command));
        if !stdout.trim().is_empty() {
            result.push_str(stdout.trim());
            result.push('\n');
        }
        if !stderr.trim().is_empty() {
            result.push_str("[stderr] ");
            result.push_str(stderr.trim());
            result.push('\n');
        }
        if exit != 0 {
            result.push_str(&format!("[exit code {}]\n", exit));
        }
        if result.trim().is_empty() {
            result = format!("[{}] [no output]", shell_name);
        }

        // Record in buffer and update last_used
        let mut shells = self.shells.lock().await;
        if let Some(shell) = shells.get_mut(shell_name) {
            shell.push_output(&result);
            shell.last_used = std::time::Instant::now();
        }

        Ok(result)
    }

    /// Read any accumulated output from a shell's background process + buffer.
    pub async fn read_output(&self, shell_name: &str, clear: bool) -> Result<String> {
        let mut shells = self.shells.lock().await;
        let shell = shells
            .get_mut(shell_name)
            .ok_or_else(|| anyhow!("Shell '{}' not found", shell_name))?;

        // Drain any available stdout/stderr from a running background child
        let mut drained = String::new();
        let mut exit_marker: Option<String> = None;
        let mut clear_bg = false;
        if let Some(child) = shell.background.as_mut() {
            use tokio::io::AsyncReadExt;
            if let Some(out) = child.stdout.as_mut() {
                let mut buf = [0u8; 4096];
                loop {
                    let ready =
                        tokio::time::timeout(Duration::from_millis(30), out.read(&mut buf)).await;
                    match ready {
                        Ok(Ok(0)) => break,
                        Ok(Ok(n)) => drained.push_str(&String::from_utf8_lossy(&buf[..n])),
                        Ok(Err(_)) | Err(_) => break,
                    }
                }
            }
            if let Some(err) = child.stderr.as_mut() {
                let mut buf = [0u8; 4096];
                loop {
                    let ready =
                        tokio::time::timeout(Duration::from_millis(30), err.read(&mut buf)).await;
                    match ready {
                        Ok(Ok(0)) => break,
                        Ok(Ok(n)) => drained.push_str(&String::from_utf8_lossy(&buf[..n])),
                        Ok(Err(_)) | Err(_) => break,
                    }
                }
            }
            // Check if the child has exited (while we still hold the borrow)
            match child.try_wait() {
                Ok(Some(status)) => {
                    exit_marker = Some(format!("\n[background exited: {}]\n", status));
                    clear_bg = true;
                }
                Ok(None) => {}
                Err(e) => {
                    exit_marker = Some(format!("\n[wait error: {}]\n", e));
                }
            }
        }

        // Now we're done with the child; safe to mutate shell state.
        if !drained.is_empty() {
            shell.push_output(&drained);
        }
        if let Some(marker) = exit_marker {
            shell.push_output(&marker);
        }
        if clear_bg {
            shell.background = None;
        }

        let out = if shell.output_buffer.is_empty() {
            format!("[{}] (no output yet)", shell_name)
        } else {
            shell.output_buffer.clone()
        };

        if clear {
            shell.output_buffer.clear();
        }
        Ok(out)
    }

    /// List all active shells with their cwd and background state.
    pub async fn list(&self) -> String {
        let shells = self.shells.lock().await;
        if shells.is_empty() {
            return "No active shells.".to_string();
        }
        let mut out = format!("Active shells ({}):\n", shells.len());
        let mut names: Vec<&String> = shells.keys().collect();
        names.sort();
        for name in names {
            let s = &shells[name];
            let age = s.last_used.elapsed().as_secs();
            let bg = if s.background.is_some() {
                "background running"
            } else {
                "idle"
            };
            out.push_str(&format!(
                "  • {} - cwd={} - {} - last used {}s ago - {} buf chars\n",
                s.name,
                s.cwd.display(),
                bg,
                age,
                s.output_buffer.len()
            ));
        }
        out
    }

    /// Kill a shell: terminate any background process and drop it.
    pub async fn kill(&self, shell_name: &str) -> Result<String> {
        let mut shells = self.shells.lock().await;
        let mut shell = shells
            .remove(shell_name)
            .ok_or_else(|| anyhow!("Shell '{}' not found", shell_name))?;
        if let Some(mut child) = shell.background.take() {
            let _ = child.kill().await;
        }
        Ok(format!("Killed shell '{}'", shell_name))
    }

    /// Kill all shells (called on shutdown).
    #[allow(dead_code)]
    pub async fn kill_all(&self) {
        let mut shells = self.shells.lock().await;
        for (_, mut shell) in shells.drain() {
            if let Some(mut child) = shell.background.take() {
                let _ = child.kill().await;
            }
        }
    }
}

impl Default for ShellManager {
    fn default() -> Self {
        Self::new()
    }
}

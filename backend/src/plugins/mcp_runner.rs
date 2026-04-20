// MCP entry-point detection and subprocess runner.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use tokio::process::{Child, Command};
use tracing::{debug, info, warn};

// MCP config schema (standard mcpServers format) 

/// A single MCP server entry - standard `mcpServers` schema used by MCP hosts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub command: String,
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
}

impl McpServerConfig {
    /// Render as a single shell-style command string for storage / display.
    pub fn to_entry_point_string(&self) -> String {
        if self.args.is_empty() {
            self.command.clone()
        } else {
            format!("{} {}", self.command, self.args.join(" "))
        }
    }

    /// Parse from an entry-point string (space-separated command + args).
    pub fn from_entry_point_string(s: &str) -> Self {
        let mut parts = s.splitn(2, ' ');
        let command = parts.next().unwrap_or("").to_string();
        let args_str = parts.next().unwrap_or("");
        let args = if args_str.is_empty() {
            vec![]
        } else {
            args_str.split_whitespace().map(str::to_string).collect()
        };
        Self {
            command,
            args,
            env: HashMap::new(),
        }
    }
}

/// Top-level `mcp.json` written to the install directory.
#[derive(Debug, Serialize, Deserialize)]
pub struct McpConfigFile {
    /// Plugin identifier (owner/repo).
    pub plugin_id: String,
    /// Standard mcpServers block (MCP protocol format).
    pub mcp_servers: HashMap<String, McpServerConfig>,
}

// Entry-point detection 

/// Detect how to run an MCP server from its install directory.
///
/// Returns `(entry_point_string, McpServerConfig)` - the string is for DB
/// storage; the config is written to `mcp.json`.
pub async fn detect_mcp_config(
    install_path: &Path,
    _plugin_id: &str,
) -> Option<(String, McpServerConfig)> {
 // 0a. Official MCP Registry server.json (highest priority) 
    // Schema: https://static.modelcontextprotocol.io/schemas/*/server.schema.json
    // Contains the exact registry package (npm/pypi) + env var requirements.
    let server_json_path = install_path.join("server.json");
    if server_json_path.exists() {
        if let Ok(text) = tokio::fs::read_to_string(&server_json_path).await {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                // Only treat as MCP registry format if the $schema URL matches
                let is_mcp_schema = v
                    .get("$schema")
                    .and_then(|s| s.as_str())
                    .map(|s| s.contains("modelcontextprotocol.io"))
                    .unwrap_or(false);

                if is_mcp_schema {
                    if let Some(packages) = v.get("packages").and_then(|p| p.as_array()) {
                        // Prefer npm packages (stdio transport)
                        for pkg in packages {
                            let registry = pkg
                                .get("registryType")
                                .and_then(|r| r.as_str())
                                .unwrap_or("");
                            let transport = pkg
                                .pointer("/transport/type")
                                .and_then(|t| t.as_str())
                                .unwrap_or("stdio");
                            if registry == "npm" && transport == "stdio" {
                                if let Some(identifier) =
                                    pkg.get("identifier").and_then(|i| i.as_str())
                                {
                                    // Collect optional env var names for the mcp.json template
                                    let env: HashMap<String, String> = pkg
                                        .get("environmentVariables")
                                        .and_then(|e| e.as_array())
                                        .unwrap_or(&vec![])
                                        .iter()
                                        .filter_map(|ev| {
                                            let name = ev.get("name").and_then(|n| n.as_str())?;
                                            let required = ev
                                                .get("isRequired")
                                                .and_then(|r| r.as_bool())
                                                .unwrap_or(false);
                                            if required {
                                                Some((name.to_string(), String::new()))
                                            } else {
                                                None
                                            }
                                        })
                                        .collect();
                                    let cfg = McpServerConfig {
                                        command: "npx".into(),
                                        args: vec!["-y".into(), identifier.to_string()],
                                        env,
                                    };
                                    let ep = cfg.to_entry_point_string();
                                    info!("[detect] server.json npm '{}' → {}", identifier, ep);
                                    return Some((ep, cfg));
                                }
                            }
                        }
                        // Fallback to pypi
                        for pkg in packages {
                            let registry = pkg
                                .get("registryType")
                                .and_then(|r| r.as_str())
                                .unwrap_or("");
                            if registry == "pypi" {
                                if let Some(identifier) =
                                    pkg.get("identifier").and_then(|i| i.as_str())
                                {
                                    let cfg = McpServerConfig {
                                        command: "uvx".into(),
                                        args: vec![identifier.to_string()],
                                        env: HashMap::new(),
                                    };
                                    let ep = cfg.to_entry_point_string();
                                    info!(
                                        "[detect] server.json pypi '{}' → uvx {}",
                                        identifier, ep
                                    );
                                    return Some((ep, cfg));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

 // 0b. Existing mcp.json 
    let mcp_json_path = install_path.join("mcp.json");
    if mcp_json_path.exists() {
        if let Ok(text) = tokio::fs::read_to_string(&mcp_json_path).await {
            if let Ok(cfg) = serde_json::from_str::<McpConfigFile>(&text) {
                if let Some(srv) = cfg.mcp_servers.values().next() {
                    let ep = srv.to_entry_point_string();
                    info!("[detect] using existing mcp.json entry_point={}", ep);
                    return Some((ep, srv.clone()));
                }
            }
            // Also try standard MCP host format: {"mcpServers": {"name": {...}}}
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(servers) = v.get("mcpServers").and_then(|s| s.as_object()) {
                    if let Some((_name, srv_val)) = servers.iter().next() {
                        if let Ok(srv) = serde_json::from_value::<McpServerConfig>(srv_val.clone())
                        {
                            let ep = srv.to_entry_point_string();
                            info!("[detect] using existing mcp.json entry_point={}", ep);
                            return Some((ep, srv));
                        }
                    }
                }
            }
        }
    }

 // 1. Node.js 
    let pkg_path = install_path.join("package.json");
    if pkg_path.exists() {
        if let Ok(text) = tokio::fs::read_to_string(&pkg_path).await {
            if let Ok(json) = serde_json::from_str::<Value>(&text) {
                let pkg_name = json.get("name").and_then(|v| v.as_str()).unwrap_or("");

                // Prefer npx -y <name> for published packages.
                // Heuristic: name is non-empty and doesn't look like a private/local name.
                if !pkg_name.is_empty()
                    && !pkg_name.starts_with('_')
                    && !pkg_name.contains("private")
                    && !pkg_name.contains("internal")
                {
                    let cfg = McpServerConfig {
                        command: "npx".into(),
                        args: vec!["-y".into(), pkg_name.to_string()],
                        env: HashMap::new(),
                    };
                    let ep = cfg.to_entry_point_string();
                    info!("[detect] Node pkg '{}' → {}", pkg_name, ep);
                    return Some((ep, cfg));
                }

                // Explicit bin field
                if let Some(bin) = json.get("bin") {
                    let script = match bin {
                        Value::String(s) => Some(s.as_str()),
                        Value::Object(m) => m.values().next().and_then(|v| v.as_str()),
                        _ => None,
                    };
                    if let Some(s) = script {
                        let cfg = McpServerConfig {
                            command: "node".into(),
                            args: vec![s.to_string()],
                            env: HashMap::new(),
                        };
                        let ep = cfg.to_entry_point_string();
                        info!("[detect] Node bin → {}", ep);
                        return Some((ep, cfg));
                    }
                }

                // scripts.start / scripts.mcp
                for key in &["mcp", "start"] {
                    if let Some(start) = json
                        .pointer(&format!("/scripts/{}", key))
                        .and_then(|v| v.as_str())
                    {
                        let cfg = McpServerConfig::from_entry_point_string(start);
                        let ep = cfg.to_entry_point_string();
                        info!("[detect] Node scripts.{} → {}", key, ep);
                        return Some((ep, cfg));
                    }
                }

                // Common built output paths
                for candidate in &[
                    "dist/index.js",
                    "build/index.js",
                    "out/index.js",
                    "dist/server.js",
                    "build/server.js",
                    "src/index.js",
                    "index.js",
                ] {
                    if install_path.join(candidate).exists() {
                        let cfg = McpServerConfig {
                            command: "node".into(),
                            args: vec![candidate.to_string()],
                            env: HashMap::new(),
                        };
                        let ep = cfg.to_entry_point_string();
                        info!("[detect] Node file {} → {}", candidate, ep);
                        return Some((ep, cfg));
                    }
                }

                // main field
                if let Some(main) = json.get("main").and_then(|v| v.as_str()) {
                    let cfg = McpServerConfig {
                        command: "node".into(),
                        args: vec![main.to_string()],
                        env: HashMap::new(),
                    };
                    let ep = cfg.to_entry_point_string();
                    info!("[detect] Node main → {}", ep);
                    return Some((ep, cfg));
                }
            }
        }
    }

 // 2. Python 
    let pyproject = install_path.join("pyproject.toml");
    let setup_py = install_path.join("setup.py");
    let setup_cfg = install_path.join("setup.cfg");

    if pyproject.exists() || setup_py.exists() || setup_cfg.exists() {
        // Try to extract package name from pyproject.toml
        let mut py_pkg_name: Option<String> = None;

        if pyproject.exists() {
            if let Ok(text) = tokio::fs::read_to_string(&pyproject).await {
                // Simple line scan: `name = "my-package"` under [project] or [tool.poetry]
                for line in text.lines() {
                    let trimmed = line.trim();
                    if trimmed.starts_with("name") && trimmed.contains('=') {
                        if let Some(name) = trimmed
                            .split_once('=')
                            .map(|(_, v)| v.trim().trim_matches('"').trim_matches('\'').to_string())
                        {
                            if !name.is_empty() {
                                py_pkg_name = Some(name);
                                break;
                            }
                        }
                    }
                }

                // Check for [project.scripts] or [tool.poetry.scripts] entries
                let mut in_scripts = false;
                for line in text.lines() {
                    let trimmed = line.trim();
                    if trimmed.starts_with('[') {
                        in_scripts = trimmed.contains("scripts");
                        continue;
                    }
                    if in_scripts && trimmed.contains('=') && !trimmed.starts_with('#') {
                        let script_name = trimmed.split('=').next().unwrap_or("").trim();
                        if !script_name.is_empty() {
                            let cfg = McpServerConfig {
                                command: "python".into(),
                                args: vec!["-m".into(), script_name.replace('-', "_")],
                                env: HashMap::new(),
                            };
                            let ep = cfg.to_entry_point_string();
                            info!("[detect] Python script entry '{}' → {}", script_name, ep);
                            return Some((ep, cfg));
                        }
                    }
                }
            }
        }

        // Prefer uvx <name> if we have a package name and uvx is available
        if let Some(ref name) = py_pkg_name {
            if which_available("uvx").await {
                let cfg = McpServerConfig {
                    command: "uvx".into(),
                    args: vec![name.clone()],
                    env: HashMap::new(),
                };
                let ep = cfg.to_entry_point_string();
                info!("[detect] Python pkg '{}' → uvx {}", name, ep);
                return Some((ep, cfg));
            }

            // python -m <module-name> (replace hyphens with underscores for module syntax)
            let module = name.replace('-', "_");
            let cfg = McpServerConfig {
                command: "python".into(),
                args: vec!["-m".into(), module],
                env: HashMap::new(),
            };
            let ep = cfg.to_entry_point_string();
            info!("[detect] Python pkg '{}' → python -m", name);
            return Some((ep, cfg));
        }

        // Fall back to common entry-point files
        for candidate in &[
            "server.py",
            "main.py",
            "app.py",
            "src/server.py",
            "src/main.py",
        ] {
            if install_path.join(candidate).exists() {
                let cfg = McpServerConfig {
                    command: "python".into(),
                    args: vec![candidate.to_string()],
                    env: HashMap::new(),
                };
                let ep = cfg.to_entry_point_string();
                info!("[detect] Python file {} → {}", candidate, ep);
                return Some((ep, cfg));
            }
        }
    }

 // 3. Rust binary 
    let cargo_toml = install_path.join("Cargo.toml");
    if cargo_toml.exists() {
        if let Ok(text) = tokio::fs::read_to_string(&cargo_toml).await {
            for line in text.lines() {
                if line.trim_start().starts_with("name") {
                    if let Some(name) = line.split('"').nth(1) {
                        for ext in &["", ".exe"] {
                            let bin = install_path
                                .join("target")
                                .join("release")
                                .join(format!("{}{}", name, ext));
                            if bin.exists() {
                                let cfg = McpServerConfig {
                                    command: bin.to_string_lossy().into_owned(),
                                    args: vec![],
                                    env: HashMap::new(),
                                };
                                let ep = cfg.to_entry_point_string();
                                info!("[detect] Rust binary → {}", ep);
                                return Some((ep, cfg));
                            }
                        }
                    }
                }
            }
        }
    }

 // 4. Go 
    if install_path.join("go.mod").exists() {
        for candidate in &["main.go", "cmd/server/main.go", "cmd/main.go"] {
            if install_path.join(candidate).exists() {
                let cfg = McpServerConfig {
                    command: "go".into(),
                    args: vec!["run".into(), candidate.to_string()],
                    env: HashMap::new(),
                };
                let ep = cfg.to_entry_point_string();
                info!("[detect] Go → {}", ep);
                return Some((ep, cfg));
            }
        }
    }

 // 5. Shell scripts 
    for script in &["run.sh", "start.sh", "server.sh"] {
        let p = install_path.join(script);
        if p.exists() {
            let cfg = McpServerConfig {
                command: "bash".into(),
                args: vec![script.to_string()],
                env: HashMap::new(),
            };
            let ep = cfg.to_entry_point_string();
            info!("[detect] shell script → {}", ep);
            return Some((ep, cfg));
        }
    }

    warn!("[detect] no entry point found in {:?}", install_path);
    None
}

/// Legacy wrapper - returns just the entry-point string.
pub async fn detect_entry_point(install_path: &Path) -> Option<String> {
    detect_mcp_config(install_path, "").await.map(|(ep, _)| ep)
}

/// Write `mcp.json` to the install directory in standard MCP host format.
pub async fn write_mcp_json(
    install_path: &Path,
    plugin_id: &str,
    cfg: &McpServerConfig,
) -> Result<()> {
    // Use the last segment of plugin_id as the server name (e.g. "upstash/context7" → "context7")
    let server_name = plugin_id.split('/').next_back().unwrap_or(plugin_id);

    let mut servers = HashMap::new();
    servers.insert(server_name.to_string(), cfg.clone());

    let file = McpConfigFile {
        plugin_id: plugin_id.to_string(),
        mcp_servers: servers,
    };

    let json = serde_json::to_string_pretty(&file).context("serialize mcp.json")?;

    tokio::fs::write(install_path.join("mcp.json"), json)
        .await
        .context("write mcp.json")?;

    info!(
        "[mcp.json] written for '{}': command={} args={:?}",
        plugin_id, cfg.command, cfg.args
    );
    Ok(())
}

/// Check if a program is on PATH (handles Windows .cmd wrappers via cmd /c).
async fn which_available(program: &str) -> bool {
    #[cfg(target_os = "windows")]
    let result = {
        let mut cmd = tokio::process::Command::new("cmd");
        crate::os::hide_tokio(&mut cmd)
            .args(["/c", program, "--version"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
    };

    #[cfg(not(target_os = "windows"))]
    let result = {
        let mut cmd = tokio::process::Command::new(program);
        cmd.arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
    };

    result.map(|s| s.success()).unwrap_or(false)
}

// MCP subprocess runner 

/// Represents a running MCP server child process.
pub struct McpProcess {
    pub plugin_id: String,
    pub entry_point: String,
    child: Child,
}

impl McpProcess {
    /// Spawn the MCP server at `install_path` using the given `entry_point` command.
    pub async fn spawn(
        plugin_id: &str,
        install_path: &Path,
        entry_point: &str,
        env_json: Option<&str>,
    ) -> Result<Self> {
        let cfg = McpServerConfig::from_entry_point_string(entry_point);

        // On Windows, batch files (.cmd) and shell builtins can't be spawned
        #[cfg(target_os = "windows")]
        let (real_program, real_args) = {
            let is_abs_exe = std::path::Path::new(&cfg.command)
                .extension()
                .map(|e| e.eq_ignore_ascii_case("exe"))
                .unwrap_or(false)
                && std::path::Path::new(&cfg.command).is_absolute();

            if is_abs_exe {
                (cfg.command.clone(), cfg.args.clone())
            } else {
                // Build: cmd /c <command> <args…>
                let mut args = vec!["/c".to_string(), cfg.command.clone()];
                args.extend(cfg.args.iter().cloned());
                ("cmd".to_string(), args)
            }
        };

        #[cfg(not(target_os = "windows"))]
        let (real_program, real_args) = (cfg.command.clone(), cfg.args.clone());

        let mut cmd = Command::new(&real_program);
        cmd.args(&real_args)
            .current_dir(install_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        // Inject env vars from config_json {"env": {"KEY": "val"}}
        if let Some(raw) = env_json {
            if let Ok(v) = serde_json::from_str::<Value>(raw) {
                if let Some(env_map) = v.get("env").and_then(|e| e.as_object()) {
                    for (k, val) in env_map {
                        if let Some(s) = val.as_str() {
                            cmd.env(k, s);
                        }
                    }
                }
            }
        }

        info!(
            "Spawning MCP server '{}': {} {:?}",
            plugin_id, real_program, real_args
        );
        crate::os::hide_tokio(&mut cmd);

        let child = cmd.spawn().with_context(|| {
            format!(
                "spawn MCP process '{} {:?}' in {:?}",
                real_program, real_args, install_path
            )
        })?;

        Ok(McpProcess {
            plugin_id: plugin_id.to_string(),
            entry_point: entry_point.to_string(),
            child,
        })
    }

    pub async fn kill(mut self) -> Result<()> {
        self.child.kill().await.context("kill MCP process")
    }

    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

// MCP Manager 

pub struct McpManager {
    pub processes: std::collections::HashMap<String, McpProcess>,
}

impl McpManager {
    pub fn new() -> Self {
        Self {
            processes: std::collections::HashMap::new(),
        }
    }

    pub async fn start(
        &mut self,
        plugin_id: &str,
        install_path: &Path,
        entry_point: &str,
        env_json: Option<&str>,
    ) -> Result<()> {
        if self.processes.contains_key(plugin_id) {
            debug!("MCP '{}' already running", plugin_id);
            return Ok(());
        }
        let proc = McpProcess::spawn(plugin_id, install_path, entry_point, env_json).await?;
        self.processes.insert(plugin_id.to_string(), proc);
        Ok(())
    }

    pub async fn stop(&mut self, plugin_id: &str) -> Result<()> {
        if let Some(proc) = self.processes.remove(plugin_id) {
            proc.kill().await?;
        }
        Ok(())
    }

    pub fn is_running(&mut self, plugin_id: &str) -> bool {
        self.processes
            .get_mut(plugin_id)
            .map(|p| p.is_alive())
            .unwrap_or(false)
    }

    pub fn running_ids(&self) -> Vec<String> {
        self.processes.keys().cloned().collect()
    }
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

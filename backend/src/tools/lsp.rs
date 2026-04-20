//! Lightweight LSP client for code intelligence (go-to-definition, references, hover).
//!
//! Spawns language servers lazily and communicates via JSON-RPC over stdin/stdout.
//! Supports: rust-analyzer, typescript-language-server, pylsp/pyright.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use tracing::{debug, info};

/// Map of language ID → server binary + args
fn server_config(lang: &str) -> Option<(&'static str, Vec<&'static str>)> {
    match lang {
        "rust" => Some(("rust-analyzer", vec![])),
        "typescript" | "javascript" | "typescriptreact" | "javascriptreact" => {
            Some(("typescript-language-server", vec!["--stdio"]))
        }
        "python" => Some(("pylsp", vec![])),
        _ => None,
    }
}

fn detect_language(path: &str) -> Option<&'static str> {
    let ext = Path::new(path).extension()?.to_str()?;
    match ext {
        "rs" => Some("rust"),
        "ts" | "tsx" => Some("typescript"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        "py" => Some("python"),
        _ => None,
    }
}

struct LspSession {
    _process: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: i64,
    opened_files: HashSet<String>,
}

impl LspSession {
    fn send_request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;

        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let body = serde_json::to_string(&msg)?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());

        self.stdin.write_all(header.as_bytes())?;
        self.stdin.write_all(body.as_bytes())?;
        self.stdin.flush()?;

        // Read responses - skip notifications, find our response by id
        loop {
            // Read headers
            let mut content_length: usize = 0;
            loop {
                let mut line = String::new();
                self.reader.read_line(&mut line)?;
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    break;
                }
                if let Some(len_str) = trimmed.strip_prefix("Content-Length: ") {
                    content_length = len_str.parse().unwrap_or(0);
                }
            }

            if content_length == 0 {
                anyhow::bail!("LSP response had no Content-Length");
            }

            let mut body_buf = vec![0u8; content_length];
            self.reader.read_exact(&mut body_buf)?;
            let response: Value = serde_json::from_slice(&body_buf)?;

            // Skip notifications (no "id" field)
            if response.get("id").is_none() {
                continue;
            }

            // Check if this is our response
            if response.get("id").and_then(|v| v.as_i64()) == Some(id) {
                if let Some(result) = response.get("result") {
                    return Ok(result.clone());
                } else if let Some(error) = response.get("error") {
                    anyhow::bail!("LSP error: {}", error);
                } else {
                    return Ok(Value::Null);
                }
            }
            // Not our response - keep reading
        }
    }

    fn send_notification(&mut self, method: &str, params: Value) -> Result<()> {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let body = serde_json::to_string(&msg)?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        self.stdin.write_all(header.as_bytes())?;
        self.stdin.write_all(body.as_bytes())?;
        self.stdin.flush()?;
        Ok(())
    }
}

impl Drop for LspSession {
    fn drop(&mut self) {
        let _ = self._process.kill();
    }
}

pub struct LspManager {
    sessions: Mutex<HashMap<String, LspSession>>,
}

impl LspManager {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn ensure_session(&self, lang: &str, file_path: &str) -> Result<()> {
        let mut sessions = self.sessions.lock().map_err(|e| anyhow::anyhow!("{}", e))?;
        if sessions.contains_key(lang) {
            return Ok(());
        }

        let (bin, args) = server_config(lang)
            .ok_or_else(|| anyhow::anyhow!("No LSP server configured for '{}'", lang))?;

        // Check if server binary exists
        let which = {
            let mut cmd = if cfg!(windows) {
                Command::new("where")
            } else {
                Command::new("which")
            };
            crate::os::hide(&mut cmd).arg(bin).output()
        };
        if which.map(|o| !o.status.success()).unwrap_or(true) {
            anyhow::bail!(
                "LSP server '{}' not found on PATH. Install it for {} code intelligence.",
                bin,
                lang
            );
        }

        // Determine project root from file path
        let root = find_project_root(file_path).unwrap_or_else(|| {
            Path::new(file_path)
                .parent()
                .unwrap_or(Path::new("."))
                .to_path_buf()
        });
        let root_uri = format!(
            "file:///{}",
            root.display()
                .to_string()
                .replace('\\', "/")
                .trim_start_matches('/')
        );

        info!(
            "Starting LSP server '{}' for {} (root: {})",
            bin,
            lang,
            root.display()
        );

        let mut cmd = Command::new(bin);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        crate::os::hide(&mut cmd);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("Failed to start LSP server '{}'", bin))?;

        let stdin = child.stdin.take().context("Failed to take LSP stdin")?;
        let stdout = child.stdout.take().context("Failed to take LSP stdout")?;

        let mut session = LspSession {
            _process: child,
            stdin,
            reader: BufReader::new(stdout),
            next_id: 1,
            opened_files: HashSet::new(),
        };

        // Initialize
        let _init_result = session.send_request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "capabilities": {
                    "textDocument": {
                        "definition": { "dynamicRegistration": false },
                        "references": { "dynamicRegistration": false },
                        "hover": { "contentFormat": ["plaintext"] }
                    }
                }
            }),
        )?;

        session.send_notification("initialized", json!({}))?;
        debug!("LSP {} initialized for {}", bin, lang);

        sessions.insert(lang.to_string(), session);
        Ok(())
    }

    fn open_file_if_needed(&self, lang: &str, file_path: &str) -> Result<()> {
        let mut sessions = self.sessions.lock().map_err(|e| anyhow::anyhow!("{}", e))?;
        let session = sessions
            .get_mut(lang)
            .ok_or_else(|| anyhow::anyhow!("No session for {}", lang))?;

        let uri = path_to_uri(file_path);
        if session.opened_files.contains(&uri) {
            // Already opened - send didChange with fresh content instead
            let content = std::fs::read_to_string(file_path)?;
            session.send_notification(
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": uri, "version": 2 },
                    "contentChanges": [{ "text": content }]
                }),
            )?;
            return Ok(());
        }

        let content = std::fs::read_to_string(file_path)?;
        session.send_notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": lang,
                    "version": 1,
                    "text": content,
                }
            }),
        )?;
        session.opened_files.insert(uri);
        Ok(())
    }

    /// Go to definition at a given line/character position.
    pub fn go_to_definition(&self, file_path: &str, line: u32, character: u32) -> Result<String> {
        let lang =
            detect_language(file_path).ok_or_else(|| anyhow::anyhow!("Unsupported file type"))?;
        self.ensure_session(lang, file_path)?;
        self.open_file_if_needed(lang, file_path)?;

        let mut sessions = self.sessions.lock().map_err(|e| anyhow::anyhow!("{}", e))?;
        let session = sessions.get_mut(lang).unwrap();

        let result = session.send_request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": path_to_uri(file_path) },
                "position": { "line": line, "character": character }
            }),
        )?;

        Ok(format_location_result(&result))
    }

    /// Find all references to the symbol at a given position.
    pub fn find_references(&self, file_path: &str, line: u32, character: u32) -> Result<String> {
        let lang =
            detect_language(file_path).ok_or_else(|| anyhow::anyhow!("Unsupported file type"))?;
        self.ensure_session(lang, file_path)?;
        self.open_file_if_needed(lang, file_path)?;

        let mut sessions = self.sessions.lock().map_err(|e| anyhow::anyhow!("{}", e))?;
        let session = sessions.get_mut(lang).unwrap();

        let result = session.send_request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": path_to_uri(file_path) },
                "position": { "line": line, "character": character },
                "context": { "includeDeclaration": true }
            }),
        )?;

        Ok(format_location_result(&result))
    }

    /// Get hover information (type, docs) for the symbol at a given position.
    pub fn hover(&self, file_path: &str, line: u32, character: u32) -> Result<String> {
        let lang =
            detect_language(file_path).ok_or_else(|| anyhow::anyhow!("Unsupported file type"))?;
        self.ensure_session(lang, file_path)?;
        self.open_file_if_needed(lang, file_path)?;

        let mut sessions = self.sessions.lock().map_err(|e| anyhow::anyhow!("{}", e))?;
        let session = sessions.get_mut(lang).unwrap();

        let result = session.send_request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": path_to_uri(file_path) },
                "position": { "line": line, "character": character }
            }),
        )?;

        if result.is_null() {
            return Ok("No hover information available at this position.".to_string());
        }

        if let Some(contents) = result.get("contents") {
            if let Some(s) = contents.as_str() {
                return Ok(s.to_string());
            }
            if let Some(obj) = contents.as_object() {
                if let Some(value) = obj.get("value").and_then(|v| v.as_str()) {
                    return Ok(value.to_string());
                }
            }
            if let Some(arr) = contents.as_array() {
                let parts: Vec<String> = arr
                    .iter()
                    .filter_map(|v| {
                        v.as_str()
                            .map(String::from)
                            .or_else(|| v.get("value").and_then(|v| v.as_str()).map(String::from))
                    })
                    .collect();
                return Ok(parts.join("\n\n"));
            }
        }

        Ok(serde_json::to_string_pretty(&result).unwrap_or_default())
    }

    #[allow(dead_code)]
    pub fn shutdown_all(&self) {
        if let Ok(mut sessions) = self.sessions.lock() {
            for (lang, mut session) in sessions.drain() {
                debug!("Shutting down LSP for {}", lang);
                let _ = session.send_request("shutdown", json!(null));
                let _ = session.send_notification("exit", json!(null));
            }
        }
    }
}

fn path_to_uri(path: &str) -> String {
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
    format!(
        "file:///{}",
        abs.display()
            .to_string()
            .replace('\\', "/")
            .trim_start_matches('/')
    )
}

fn find_project_root(file_path: &str) -> Option<PathBuf> {
    let mut dir = Path::new(file_path).parent()?;
    let markers = [
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "setup.py",
        ".git",
    ];
    loop {
        for m in &markers {
            if dir.join(m).exists() {
                return Some(dir.to_path_buf());
            }
        }
        dir = dir.parent()?;
    }
}

fn format_location_result(result: &Value) -> String {
    if result.is_null() {
        return "No results found.".to_string();
    }

    let locations = if result.is_array() {
        result.as_array().unwrap().clone()
    } else {
        vec![result.clone()]
    };

    if locations.is_empty() {
        return "No results found.".to_string();
    }

    let mut out = String::new();
    for loc in &locations {
        let uri = loc.get("uri").and_then(|v| v.as_str()).unwrap_or("?");
        let range = loc.get("range").and_then(|r| r.get("start"));
        let line = range
            .and_then(|r| r.get("line"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let col = range
            .and_then(|r| r.get("character"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let path = uri.strip_prefix("file:///").unwrap_or(uri);
        out.push_str(&format!("{}:{}:{}\n", path, line + 1, col + 1));
    }
    out
}

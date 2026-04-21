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

/// One cached code action from a recent `code_actions` call, keyed so the LLM
/// can apply it by id instead of shipping the entire action object back.
#[derive(Clone)]
pub struct CachedAction {
    pub id: String,
    pub title: String,
    pub action: Value, // raw LSP CodeAction | Command
    pub lang: String,
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
    // ring buffer of recent code actions (max 20). LLM references them by id.
    recent_actions: Mutex<Vec<CachedAction>>,
}

impl LspManager {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            recent_actions: Mutex::new(Vec::new()),
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
                    "workspace": {
                        "applyEdit": true,
                        "workspaceEdit": {
                            "documentChanges": true,
                            "resourceOperations": ["create","rename","delete"]
                        },
                        "symbol": { "dynamicRegistration": false },
                        "executeCommand": { "dynamicRegistration": false }
                    },
                    "textDocument": {
                        "definition": { "dynamicRegistration": false },
                        "references": { "dynamicRegistration": false },
                        "hover": { "contentFormat": ["plaintext","markdown"] },
                        "rename": { "dynamicRegistration": false, "prepareSupport": true },
                        "codeAction": {
                            "dynamicRegistration": false,
                            "codeActionLiteralSupport": {
                                "codeActionKind": {
                                    "valueSet": ["","quickfix","refactor","refactor.extract","refactor.inline","refactor.rewrite","source","source.organizeImports"]
                                }
                            }
                        }
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

    /// Rename a symbol across the workspace. Returns a human summary describing
    /// how many files/edits the server produced, then applies them to disk.
    pub fn rename(
        &self,
        file_path: &str,
        line: u32,
        character: u32,
        new_name: &str,
    ) -> Result<String> {
        let lang =
            detect_language(file_path).ok_or_else(|| anyhow::anyhow!("Unsupported file type"))?;
        self.ensure_session(lang, file_path)?;
        self.open_file_if_needed(lang, file_path)?;

        let mut sessions = self.sessions.lock().map_err(|e| anyhow::anyhow!("{}", e))?;
        let session = sessions.get_mut(lang).unwrap();

        let result = session.send_request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": path_to_uri(file_path) },
                "position": { "line": line, "character": character },
                "newName": new_name,
            }),
        )?;

        drop(sessions); // release before applying edits
        if result.is_null() {
            return Ok("Rename: server returned nothing. The symbol at this position may not be renamable.".to_string());
        }
        apply_workspace_edit(&result, &format!("rename → {}", new_name))
    }

    /// Fuzzy project-wide symbol search. Needs at least one session running, so
    /// we require a hint file to figure out which language server to ask.
    pub fn workspace_symbol(&self, lang_hint_path: &str, query: &str) -> Result<String> {
        let lang = detect_language(lang_hint_path)
            .ok_or_else(|| anyhow::anyhow!("Unsupported file type for hint path"))?;
        self.ensure_session(lang, lang_hint_path)?;

        let mut sessions = self.sessions.lock().map_err(|e| anyhow::anyhow!("{}", e))?;
        let session = sessions.get_mut(lang).unwrap();

        let result = session.send_request("workspace/symbol", json!({ "query": query }))?;

        let arr = result.as_array().cloned().unwrap_or_default();
        if arr.is_empty() {
            return Ok(format!("No symbols matching '{}'.", query));
        }

        let mut out = String::new();
        for sym in arr.iter().take(50) {
            let name = sym.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let kind = sym.get("kind").and_then(|v| v.as_u64()).unwrap_or(0);
            let loc = sym.get("location").unwrap_or(sym);
            let uri = loc.get("uri").and_then(|v| v.as_str()).unwrap_or("?");
            let line = loc
                .get("range")
                .and_then(|r| r.get("start"))
                .and_then(|s| s.get("line"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let path = uri_to_path_display(uri);
            out.push_str(&format!(
                "{} ({}) {}:{}\n",
                name,
                symbol_kind_name(kind),
                path,
                line + 1
            ));
        }
        Ok(out)
    }

    /// Ask the server for code actions at a position. Caches the results so the
    /// LLM can apply one by id without round-tripping the whole action JSON.
    pub fn code_actions(&self, file_path: &str, line: u32, character: u32) -> Result<String> {
        let lang =
            detect_language(file_path).ok_or_else(|| anyhow::anyhow!("Unsupported file type"))?;
        self.ensure_session(lang, file_path)?;
        self.open_file_if_needed(lang, file_path)?;

        let mut sessions = self.sessions.lock().map_err(|e| anyhow::anyhow!("{}", e))?;
        let session = sessions.get_mut(lang).unwrap();

        let result = session.send_request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": path_to_uri(file_path) },
                "range": {
                    "start": { "line": line, "character": character },
                    "end":   { "line": line, "character": character }
                },
                "context": { "diagnostics": [] }
            }),
        )?;

        drop(sessions);

        let arr = result.as_array().cloned().unwrap_or_default();
        if arr.is_empty() {
            return Ok("No code actions available at this position.".to_string());
        }

        let mut cache = self
            .recent_actions
            .lock()
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let base = cache.len();
        let mut listing = String::new();
        for (i, action) in arr.iter().enumerate() {
            let id = format!("a{}", base + i);
            let title = action
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("(untitled action)")
                .to_string();
            let kind_tag = action
                .get("kind")
                .and_then(|v| v.as_str())
                .map(|k| format!(" [{}]", k))
                .unwrap_or_default();
            listing.push_str(&format!("{}: {}{}\n", id, title, kind_tag));
            cache.push(CachedAction {
                id,
                title,
                action: action.clone(),
                lang: lang.to_string(),
            });
        }
        // keep only the most recent 50 so memory doesn't grow unbounded
        let overflow = cache.len().saturating_sub(50);
        if overflow > 0 {
            cache.drain(0..overflow);
        }
        Ok(listing)
    }

    /// Apply a cached code action by id (from a recent code_actions response).
    /// Resolves the action first if the server supports it, then applies the
    /// resulting WorkspaceEdit and/or executes the command.
    pub fn apply_code_action(&self, action_id: &str) -> Result<String> {
        let cached = {
            let cache = self
                .recent_actions
                .lock()
                .map_err(|e| anyhow::anyhow!("{}", e))?;
            cache
                .iter()
                .find(|a| a.id == action_id)
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "No cached action '{}'. Run code_actions first and use one of its ids.",
                        action_id
                    )
                })?
        };

        // try to resolve the action (some servers send a skeleton first, edit/command after)
        let mut sessions = self.sessions.lock().map_err(|e| anyhow::anyhow!("{}", e))?;
        let session = sessions
            .get_mut(cached.lang.as_str())
            .ok_or_else(|| anyhow::anyhow!("LSP session for '{}' dropped", cached.lang))?;

        let resolved = match session.send_request("codeAction/resolve", cached.action.clone()) {
            Ok(v) if !v.is_null() => v,
            _ => cached.action.clone(), // fall back to the original; server may not support resolve
        };

        let mut summary = format!("Applied action '{}'", cached.title);

        // 1. WorkspaceEdit under "edit" (CodeAction) or the action itself (Command-only)
        if let Some(edit) = resolved.get("edit") {
            drop(sessions);
            let applied = apply_workspace_edit(edit, &cached.title)?;
            summary.push_str("\n");
            summary.push_str(&applied);
            sessions = self.sessions.lock().map_err(|e| anyhow::anyhow!("{}", e))?;
        }

        // 2. Command to execute
        if let Some(cmd) = resolved
            .get("command")
            .or_else(|| Some(&resolved))
            .filter(|v| v.get("command").is_some())
        {
            let session = sessions.get_mut(cached.lang.as_str()).unwrap();
            let exec = session.send_request(
                "workspace/executeCommand",
                json!({
                    "command": cmd.get("command").cloned().unwrap_or(Value::Null),
                    "arguments": cmd.get("arguments").cloned().unwrap_or(Value::Array(vec![])),
                }),
            );
            if let Err(e) = exec {
                summary.push_str(&format!("\n(executeCommand failed: {})", e));
            }
        }

        Ok(summary)
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

fn uri_to_path_display(uri: &str) -> String {
    // try the url crate first for proper %-decoding. fall back to naive strip on failure.
    match url::Url::parse(uri) {
        Ok(u) => match u.to_file_path() {
            Ok(p) => p.display().to_string(),
            Err(_) => uri.to_string(),
        },
        Err(_) => uri
            .strip_prefix("file:///")
            .unwrap_or(uri.strip_prefix("file://").unwrap_or(uri))
            .to_string(),
    }
}

fn symbol_kind_name(kind: u64) -> &'static str {
    // from lsp-types SymbolKind
    match kind {
        1 => "File",
        2 => "Module",
        3 => "Namespace",
        4 => "Package",
        5 => "Class",
        6 => "Method",
        7 => "Property",
        8 => "Field",
        9 => "Constructor",
        10 => "Enum",
        11 => "Interface",
        12 => "Function",
        13 => "Variable",
        14 => "Constant",
        15 => "String",
        16 => "Number",
        17 => "Boolean",
        18 => "Array",
        19 => "Object",
        20 => "Key",
        21 => "Null",
        22 => "EnumMember",
        23 => "Struct",
        24 => "Event",
        25 => "Operator",
        26 => "TypeParameter",
        _ => "?",
    }
}

/// Apply a WorkspaceEdit to disk. Supports `changes` (map<uri, TextEdit[]>) and
/// `documentChanges` (TextDocumentEdit[]). Skips resource ops (create/rename/delete)
/// for now and reports them in the summary.
fn apply_workspace_edit(edit: &Value, label: &str) -> Result<String> {
    use std::collections::BTreeMap;

    // normalize into map<path, Vec<TextEdit>>
    let mut by_file: BTreeMap<PathBuf, Vec<Value>> = BTreeMap::new();
    let mut resource_ops: Vec<String> = Vec::new();

    if let Some(changes) = edit.get("changes").and_then(|v| v.as_object()) {
        for (uri, edits) in changes {
            let path = uri_to_pathbuf(uri)?;
            let arr = edits.as_array().cloned().unwrap_or_default();
            by_file.entry(path).or_default().extend(arr);
        }
    }

    if let Some(doc_changes) = edit.get("documentChanges").and_then(|v| v.as_array()) {
        for ch in doc_changes {
            // TextDocumentEdit { textDocument: { uri }, edits: [...] }
            if let Some(td) = ch.get("textDocument") {
                let uri = td
                    .get("uri")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("documentChanges entry has no uri"))?;
                let path = uri_to_pathbuf(uri)?;
                let arr = ch
                    .get("edits")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                by_file.entry(path).or_default().extend(arr);
            } else if let Some(kind) = ch.get("kind").and_then(|v| v.as_str()) {
                // create/rename/delete - not applied here, just reported
                resource_ops.push(kind.to_string());
            }
        }
    }

    if by_file.is_empty() && resource_ops.is_empty() {
        return Ok(format!("{}: no edits returned.", label));
    }

    let mut file_count = 0usize;
    let mut edit_count = 0usize;
    for (path, edits) in &by_file {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let applied = apply_text_edits(&text, edits)?;
        std::fs::write(path, &applied).with_context(|| format!("writing {}", path.display()))?;
        file_count += 1;
        edit_count += edits.len();
    }

    let mut summary = format!(
        "{}: {} edits across {} file{}",
        label,
        edit_count,
        file_count,
        if file_count == 1 { "" } else { "s" }
    );
    if !resource_ops.is_empty() {
        summary.push_str(&format!(
            "\n(skipped {} resource op(s): {}. apply manually if needed.)",
            resource_ops.len(),
            resource_ops.join(", ")
        ));
    }
    Ok(summary)
}

fn uri_to_pathbuf(uri: &str) -> Result<PathBuf> {
    match url::Url::parse(uri) {
        Ok(u) => u
            .to_file_path()
            .map_err(|_| anyhow::anyhow!("non-file URI: {}", uri)),
        Err(_) => Ok(PathBuf::from(
            uri.strip_prefix("file:///")
                .unwrap_or(uri.strip_prefix("file://").unwrap_or(uri)),
        )),
    }
}

/// Apply a list of LSP TextEdits to a string. Edits are sorted by end position
/// descending so earlier ranges don't shift later ones.
fn apply_text_edits(source: &str, edits: &[Value]) -> Result<String> {
    // convert each edit into (start_offset, end_offset, newText)
    let mut byte_edits: Vec<(usize, usize, String)> = Vec::with_capacity(edits.len());
    for e in edits {
        let range = e
            .get("range")
            .ok_or_else(|| anyhow::anyhow!("TextEdit missing range"))?;
        let new_text = e
            .get("newText")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let start = lsp_position_to_offset(source, range.get("start"))?;
        let end = lsp_position_to_offset(source, range.get("end"))?;
        byte_edits.push((start, end, new_text));
    }
    // sort by start desc so we can splice without adjusting offsets
    byte_edits.sort_by(|a, b| b.0.cmp(&a.0));

    let mut out = source.to_string();
    for (start, end, new_text) in byte_edits {
        if start > end || end > out.len() {
            anyhow::bail!(
                "TextEdit range out of bounds: {}..{} (source len {})",
                start,
                end,
                out.len()
            );
        }
        out.replace_range(start..end, &new_text);
    }
    Ok(out)
}

/// Convert an LSP Position (line, character) in UTF-16 code units into a byte
/// offset in `source`. Close enough for ASCII/BMP; emoji-heavy files may skew
/// slightly but handlers re-parse anyway so we'd surface an error.
fn lsp_position_to_offset(source: &str, pos: Option<&Value>) -> Result<usize> {
    let pos = pos.ok_or_else(|| anyhow::anyhow!("Position missing"))?;
    let line = pos
        .get("line")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow::anyhow!("Position.line missing"))? as usize;
    let character = pos
        .get("character")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow::anyhow!("Position.character missing"))? as usize;

    let mut offset = 0usize;
    for (i, l) in source.split_inclusive('\n').enumerate() {
        if i == line {
            // walk `character` UTF-16 units into the line
            let mut u16_count = 0usize;
            for ch in l.chars() {
                if u16_count >= character {
                    break;
                }
                let clen_u16 = ch.len_utf16();
                let clen_u8 = ch.len_utf8();
                u16_count += clen_u16;
                offset += clen_u8;
            }
            return Ok(offset);
        }
        offset += l.len();
    }
    // line past EOF: clamp to source length (e.g. append-at-end edits)
    Ok(source.len())
}

#[cfg(test)]
mod applier_tests {
    use super::*;
    use serde_json::json;

    fn te(sl: u64, sc: u64, el: u64, ec: u64, new_text: &str) -> Value {
        json!({
            "range": {
                "start": {"line": sl, "character": sc},
                "end":   {"line": el, "character": ec}
            },
            "newText": new_text
        })
    }

    #[test]
    fn simple_rename_single_line() {
        let src = "let foo = 1;";
        let out = apply_text_edits(src, &[te(0, 4, 0, 7, "bar")]).unwrap();
        assert_eq!(out, "let bar = 1;");
    }

    #[test]
    fn multiple_edits_on_same_line_descending() {
        // two renames on the same line. applier must sort by start desc so
        // offsets don't shift out from under each other.
        let src = "foo + foo";
        let edits = vec![te(0, 0, 0, 3, "bar"), te(0, 6, 0, 9, "bar")];
        let out = apply_text_edits(src, &edits).unwrap();
        assert_eq!(out, "bar + bar");
    }

    #[test]
    fn edits_across_lines() {
        let src = "a\nbb\nccc";
        // delete "bb" on line 1
        let out = apply_text_edits(src, &[te(1, 0, 1, 2, "")]).unwrap();
        assert_eq!(out, "a\n\nccc");
    }

    #[test]
    fn insert_at_position() {
        let src = "hello";
        let out = apply_text_edits(src, &[te(0, 5, 0, 5, " world")]).unwrap();
        assert_eq!(out, "hello world");
    }

    #[test]
    fn character_past_eol_clamps_to_line_end() {
        // LSP servers commonly send end-of-line as a large character offset.
        // we walk chars and stop when the line runs out, so this replaces the
        // whole line rather than failing.
        let src = "short";
        let out = apply_text_edits(src, &[te(0, 0, 0, 999, "x")]).unwrap();
        assert_eq!(out, "x");
    }

    #[test]
    fn line_past_eof_clamps_to_end() {
        let src = "one\ntwo";
        let out = apply_text_edits(src, &[te(99, 0, 99, 0, "\nthree")]).unwrap();
        assert_eq!(out, "one\ntwo\nthree");
    }

    #[test]
    fn symbol_kind_known_values() {
        assert_eq!(symbol_kind_name(12), "Function");
        assert_eq!(symbol_kind_name(5), "Class");
        assert_eq!(symbol_kind_name(999), "?");
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

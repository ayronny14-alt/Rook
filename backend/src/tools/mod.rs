pub mod browser_automation;
pub mod browser_cdp;
pub mod browser_tool;
pub mod code_edit;
pub mod file_read;
pub mod file_write;
pub mod git;
pub mod lsp;
pub mod shell_manager;
pub mod terminal;
pub mod web_search;

use anyhow::Result;
use std::path::PathBuf;
#[allow(dead_code)]
use std::sync::Arc;

/// Rejects null bytes and bare path-traversal prefixes (e.g. `../../etc/passwd`).
/// Uses `path_clean` to normalize before checking.
pub(crate) fn validate_path(path: &str) -> anyhow::Result<PathBuf> {
    if path.contains('\0') {
        anyhow::bail!("Path contains null bytes");
    }
    let cleaned = path_clean::clean(path);
    if cleaned.starts_with("..") {
        anyhow::bail!("Path traversal rejected: {}", path);
    }
    Ok(cleaned)
}

/// Stricter validation for file *write* operations. Rejects writes to
/// sensitive system directories while still allowing the agent to write
/// anywhere the user has navigated to via `change_dir`.
pub(crate) fn validate_write_path(path: &str) -> anyhow::Result<PathBuf> {
    let cleaned = validate_path(path)?;

    // Block known dangerous directories regardless of cwd
    let abs = if cleaned.is_absolute() {
        cleaned.clone()
    } else {
        std::env::current_dir().unwrap_or_default().join(&cleaned)
    };
    let abs_str = abs.display().to_string().replace('\\', "/").to_lowercase();

    let blocked = [
        "/windows/system32",
        "/windows/syswow64",
        "/program files",
        "/program files (x86)",
        "/etc",
        "/usr/bin",
        "/usr/sbin",
        "/sbin",
        "/bin",
        "/boot",
        "/proc",
        "/sys",
        "/dev",
    ];
    for prefix in blocked {
        if abs_str.contains(prefix) {
            anyhow::bail!(
                "Write to '{}' blocked - system directory. Move to a project directory first.",
                path
            );
        }
    }

    // Reject symlinks - prevent following them to protected locations
    if abs.exists() {
        if let Ok(meta) = std::fs::symlink_metadata(&abs) {
            if meta.file_type().is_symlink() {
                anyhow::bail!("Write to '{}' blocked - target is a symbolic link.", path);
            }
        }
    }
    // Also check parent path components for symlinks
    if let Some(parent) = abs.parent() {
        for ancestor in parent.ancestors() {
            if ancestor.as_os_str().is_empty() {
                break;
            }
            if let Ok(m) = std::fs::symlink_metadata(ancestor) {
                if m.file_type().is_symlink() {
                    anyhow::bail!(
                        "Write to '{}' blocked - path contains a symbolic link at '{}'.",
                        path,
                        ancestor.display(),
                    );
                }
            }
        }
    }

    // Return the absolute path to prevent CWD-dependent resolution later
    Ok(abs)
}
use tokio::sync::Mutex;

use crate::memory::storage::MemoryStorage;
use crate::tools::browser_tool::BrowserTool;
use crate::tools::code_edit::CodeEditTool;
use crate::tools::file_read::FileReadTool;
use crate::tools::file_write::FileWriteTool;
use crate::tools::git::GitTool;
use crate::tools::lsp::LspManager;
use crate::tools::shell_manager::ShellManager;
use crate::tools::terminal::TerminalTool;
use crate::tools::web_search::WebSearchTool;

#[allow(dead_code)]
#[derive(Clone)]
pub struct ToolExecutor {
    file_read: Arc<FileReadTool>,
    file_write: Arc<FileWriteTool>,
    web_search: Arc<WebSearchTool>,
    code_edit: Arc<CodeEditTool>,
    terminal: Arc<TerminalTool>,
    pub shells: ShellManager,
    git: Arc<GitTool>,
    browser_tool: Arc<Mutex<BrowserTool>>,
    pub lsp: Arc<LspManager>,
    memory: Option<Arc<MemoryStorage>>,
}

impl ToolExecutor {
    pub fn new() -> Self {
        Self {
            file_read: Arc::new(FileReadTool),
            file_write: Arc::new(FileWriteTool),
            web_search: Arc::new(WebSearchTool::new()),
            code_edit: Arc::new(CodeEditTool),
            terminal: Arc::new(TerminalTool),
            shells: ShellManager::new(),
            git: Arc::new(GitTool),
            browser_tool: Arc::new(Mutex::new(BrowserTool::new())),
            lsp: Arc::new(LspManager::new()),
            memory: None,
        }
    }

    /// Attach a memory storage instance so the tool executor can
    /// auto-index navigated pages via [`BrowserIndexer`].
    pub fn with_memory(mut self, memory: MemoryStorage) -> Self {
        self.memory = Some(Arc::new(memory));
        self
    }

    pub async fn spawn_browser(&self, headless: bool) -> Result<String> {
        let mut guard = self.browser_tool.lock().await;
        guard.spawn_automation(headless)
    }

    pub async fn spawn_cdp_browser(&self, headless: bool) -> Result<String> {
        let mut guard = self.browser_tool.lock().await;
        guard.spawn_cdp(headless)
    }

    pub async fn cdp_navigate(&self, url: &str) -> Result<String> {
        let content = {
            let mut guard = self.browser_tool.lock().await;
            guard.navigate(url)?
        };

        // Auto-index the navigated page in background so it appears in memory.
        if let Some(mem) = self.memory.clone() {
            let url_owned = url.to_string();
            let content_owned = content.clone();
            tokio::spawn(async move {
                let indexer = crate::indexer::browser_indexer::BrowserIndexer::new((*mem).clone());
                // Use the first non-empty line as the title, fall back to URL.
                let title = content_owned
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or(&url_owned)
                    .trim()
                    .chars()
                    .take(120)
                    .collect::<String>();
                if let Err(e) = indexer.index_url(&url_owned, &title, &content_owned).await {
                    tracing::warn!("BrowserIndexer failed for {}: {}", url_owned, e);
                }
            });
        }

        Ok(content)
    }

    pub async fn cdp_click(&self, selector: &str) -> Result<()> {
        let mut guard = self.browser_tool.lock().await;
        guard.click(selector)
    }

    pub async fn cdp_type(&self, selector: &str, text: &str) -> Result<()> {
        let mut guard = self.browser_tool.lock().await;
        guard.type_str(selector, text)
    }

    pub async fn cdp_evaluate(&self, js: &str) -> Result<String> {
        let mut guard = self.browser_tool.lock().await;
        guard.evaluate(js)
    }

    pub async fn cdp_screenshot_base64(&self, full: bool) -> Result<String> {
        let mut guard = self.browser_tool.lock().await;
        guard.screenshot_base64(full)
    }

    pub async fn cdp_debugging_url(&self) -> Option<String> {
        let guard = self.browser_tool.lock().await;
        guard.debugging_url()
    }

    pub async fn kill_cdp_browser(&self) -> Result<()> {
        let mut guard = self.browser_tool.lock().await;
        guard.kill_all()
    }

    pub async fn browser_debugging_url(&self) -> Option<String> {
        let guard = self.browser_tool.lock().await;
        guard.debugging_url()
    }

    pub async fn kill_browser(&self) -> Result<()> {
        let mut guard = self.browser_tool.lock().await;
        guard.kill_all()
    }

    pub async fn read_file(&self, path: &str) -> Result<String> {
        self.file_read.execute(path).await
    }

    pub async fn write_file(&self, path: &str, content: &str) -> Result<()> {
        self.file_write.execute(path, content).await
    }

    pub async fn web_search(&self, query: &str) -> Result<Vec<serde_json::Value>> {
        self.web_search.execute(query).await
    }

    pub async fn fetch_url(&self, url: &str) -> Result<String> {
        self.web_search.fetch_url(url).await
    }

    pub async fn code_search_replace(
        &self,
        path: &str,
        search: &str,
        replace: &str,
    ) -> Result<String> {
        self.code_edit.search_replace(path, search, replace).await
    }

    pub async fn code_insert_at_line(
        &self,
        path: &str,
        line_number: usize,
        content: &str,
    ) -> Result<String> {
        self.code_edit
            .insert_at_line(path, line_number, content)
            .await
    }

    pub async fn code_append(&self, path: &str, content: &str) -> Result<String> {
        self.code_edit.append(path, content).await
    }

    pub async fn terminal_execute(&self, command: &str) -> Result<String> {
        self.terminal.execute(command).await
    }

    pub async fn git_status(&self, path: Option<&str>) -> Result<String> {
        self.git.status(path).await
    }

    pub async fn git_log(&self, n: usize) -> Result<String> {
        self.git.log(n, None).await
    }

    pub async fn git_diff(&self, file_path: Option<&str>) -> Result<String> {
        self.git.diff(file_path).await
    }

    pub async fn git_branch(&self) -> Result<String> {
        self.git.branch().await
    }

    pub async fn git_commit(&self, message: &str, files: &[&str]) -> Result<String> {
        self.git.commit(message, files).await
    }

    /// Compute a proper unified diff between `original` and `modified` using LCS.
    pub fn compute_diff(original: &str, modified: &str, label: &str) -> String {
        use similar::TextDiff;
        let diff = TextDiff::from_lines(original, modified);
        let mut out = format!("--- a/{}\n+++ b/{}\n", label, label);
        for hunk in diff.unified_diff().context_radius(3).iter_hunks() {
            out.push_str(&format!("{}", hunk));
        }
        out
    }
}

use anyhow::{Context, Result};
use tracing::{debug, info};

use crate::memory::graph::{GraphMemory, NodeType};
use crate::memory::object::ObjectMemoryStore;
use crate::memory::storage::MemoryStorage;

pub struct UiIndexer {
    memory: MemoryStorage,
}

impl UiIndexer {
    pub fn new(memory: MemoryStorage) -> Self {
        Self { memory }
    }

    /// Capture the title and process name of the current foreground window on
    /// Windows using an inline PowerShell / Win32 P/Invoke call.  Returns a
    /// compact JSON string like `{"title":"…","process":"…","pid":1234}`, or
    /// `None` if no foreground window could be identified.
    pub async fn capture_window(&self) -> Result<Option<String>> {
        debug!("Capturing foreground window via PowerShell");

        // Inline C# P/Invoke exposed through PowerShell Add-Type.
        // The script is intentionally defensive: any exception returns '{}'.
        let script = r#"
try {
    Add-Type @"
        using System;
        using System.Runtime.InteropServices;
        public class _RookFgWin {
            [DllImport("user32.dll")]
            public static extern IntPtr GetForegroundWindow();
            [DllImport("user32.dll", CharSet=CharSet.Unicode)]
            public static extern int GetWindowText(IntPtr hWnd, System.Text.StringBuilder sb, int n);
            [DllImport("user32.dll")]
            public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
        }
"@ -ErrorAction SilentlyContinue
    $hwnd = [_RookFgWin]::GetForegroundWindow()
    $sb   = New-Object System.Text.StringBuilder 512
    [_RookFgWin]::GetWindowText($hwnd, $sb, 512) | Out-Null
    $pid  = [uint32]0
    [_RookFgWin]::GetWindowThreadProcessId($hwnd, [ref]$pid) | Out-Null
    $proc = Get-Process -Id ([int]$pid) -ErrorAction SilentlyContinue
    @{
        title   = $sb.ToString()
        process = if ($proc) { $proc.ProcessName } else { 'unknown' }
        pid     = [int]$pid
    } | ConvertTo-Json -Compress
} catch {
    Write-Output '{}'
}
"#;

        let output = tokio::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .await
            .context("Failed to spawn PowerShell for window capture")?;

        let json_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if json_str.is_empty() || json_str == "{}" {
            return Ok(None);
        }

        // Validate it parsed as a JSON object with a non-empty title.
        let parsed: serde_json::Value =
            serde_json::from_str(&json_str).unwrap_or(serde_json::Value::Null);
        if parsed
            .get("title")
            .and_then(|t| t.as_str())
            .map(|t| t.is_empty())
            .unwrap_or(true)
        {
            return Ok(None);
        }

        Ok(Some(json_str))
    }

    /// Capture the foreground window and immediately persist it as a `UiState`
    /// node in memory.  Returns the node id if something was captured.
    pub async fn capture_and_index(&self) -> Result<Option<String>> {
        match self.capture_window().await? {
            None => Ok(None),
            Some(json_str) => {
                let parsed: serde_json::Value =
                    serde_json::from_str(&json_str).unwrap_or(serde_json::Value::Null);
                let title = parsed
                    .get("title")
                    .and_then(|t| t.as_str())
                    .unwrap_or("Unknown Window")
                    .to_string();
                self.index_ui_state(&title, &json_str).await?;
                Ok(Some(title))
            }
        }
    }

    pub async fn index_ui_state(&self, window_title: &str, window_info: &str) -> Result<()> {
        let graph = GraphMemory::new(self.memory.clone());
        let obj_store = ObjectMemoryStore::new(self.memory.clone());

        let metadata = serde_json::json!({
            "window_title": window_title,
        });

        let node = graph.create_node(NodeType::UiState, window_title, Some(metadata))?;

        let mut obj = ObjectMemoryStore::create_for_node(&node.id);
        obj.summary = Some(format!("UI state for window: {}", window_title));
        obj.ui_snapshot = Some(serde_json::json!({
            "title": window_title,
            "info": window_info,
        }));

        obj_store.upsert(&node.id, &obj)?;

        info!("Indexed UI state: {}", window_title);
        Ok(())
    }

    pub async fn get_recent_ui_states(&self, limit: usize) -> Result<Vec<serde_json::Value>> {
        let graph = GraphMemory::new(self.memory.clone());
        let nodes = graph.search_nodes(Some(NodeType::UiState), None)?;

        let results: Vec<serde_json::Value> = nodes
            .into_iter()
            .take(limit)
            .filter_map(|n| serde_json::to_value(&n).ok())
            .collect();

        Ok(results)
    }
}

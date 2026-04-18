// Dedicated structured error log.
//
// Writes one JSON-line entry per error to %LOCALAPPDATA%/Rook/error.log,
// capturing enough metadata (model, provider, mode, prompt preview, token
// estimate) to reproduce and debug without scrolling through rook.log.
//
// Usage:
//   error_log::record(ErrorContext {
//       source: "llm_api",
//       message: "413 Payload Too Large",
//       conversation_id: Some(conv_id),
//       model: Some(&model_name),
//       provider: Some("groq"),
//       agent_mode: Some("chat"),
//       prompt_messages: Some(&messages),
//       max_tokens: Some(2048),
//       extra: None,
//   });

use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

#[derive(Serialize)]
pub struct ErrorContext<'a> {
    /// Short tag identifying where the error originated.
    /// Examples: "llm_api", "ipc", "tool::file_write", "memory::embedding".
    pub source: &'a str,
    /// Human-readable error message (typically anyhow or reqwest text).
    pub message: &'a str,
    pub conversation_id: Option<&'a str>,
    pub model: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub agent_mode: Option<&'a str>,
    pub max_tokens: Option<u32>,
    /// A preview of the messages being dispatched. Each message is
    /// truncated to 300 chars so the log stays readable while still
    /// letting us reproduce the triggering input.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_preview: Option<Vec<PromptPreviewEntry>>,
    /// Estimated input tokens (chars/4) at dispatch time.
    pub est_input_tokens: Option<usize>,
    /// Anything else worth capturing (request_id, http status, retry_after).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
}

#[derive(Serialize)]
pub struct PromptPreviewEntry {
    pub role: String,
    pub chars: usize,
    pub head: String,
}

#[derive(Serialize)]
struct Record<'a> {
    timestamp: String,
    #[serde(flatten)]
    ctx: &'a ErrorContext<'a>,
}

/// Convert a slice of [`crate::llm::types::Message`] into the truncated preview
/// form safe to store on disk.
pub fn preview_messages(messages: &[crate::llm::types::Message]) -> Vec<PromptPreviewEntry> {
    messages
        .iter()
        .map(|m| PromptPreviewEntry {
            role: m.role.clone(),
            chars: m.content.len(),
            head: m.content.chars().take(300).collect::<String>(),
        })
        .collect()
}

/// Append one JSON-line record to error.log. Errors during logging are
/// swallowed silently — we never want logging to take down the main path.
pub fn record(ctx: ErrorContext<'_>) {
    let timestamp = chrono_rfc3339();
    let rec = Record {
        timestamp,
        ctx: &ctx,
    };

    if let Ok(line) = serde_json::to_string(&rec) {
        if let Some(path) = error_log_path() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(file) = writer_for(path) {
                let mut w = file.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                let _ = writeln!(w, "{}", line);
                let _ = w.flush();
            }
        }
    }
}

fn error_log_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|p| p.join("Rook").join("error.log"))
}

fn writer_for(path: PathBuf) -> std::io::Result<&'static Mutex<std::fs::File>> {
    static WRITER: OnceLock<Mutex<std::fs::File>> = OnceLock::new();
    if let Some(w) = WRITER.get() {
        return Ok(w);
    }
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    let _ = WRITER.set(Mutex::new(file));
    Ok(WRITER.get().expect("set above"))
}

// chrono is already transitively in the tree; format an RFC3339 timestamp
// in UTC manually to avoid pulling the full crate just for this helper.
fn chrono_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Minimal ISO-8601 without a chrono dep. Good enough for ordering.
    let (y, mo, d, h, mi, s) = unix_to_utc(now);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, mi, s)
}

// Plain Gregorian date computation for a Unix epoch second (no leap seconds).
fn unix_to_utc(secs: u64) -> (i32, u32, u32, u32, u32, u32) {
    let days = (secs / 86400) as i64;
    let sod = (secs % 86400) as u32;
    let h = sod / 3600;
    let mi = (sod % 3600) / 60;
    let s = sod % 60;

    // Shift epoch to 0000-03-01 so leap-day handling is uniform.
    // Algorithm: Howard Hinnant's date.
    let z = days + 719468;
    let era = if z >= 0 {
        z / 146097
    } else {
        (z - 146096) / 146097
    };
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d, h, mi, s)
}

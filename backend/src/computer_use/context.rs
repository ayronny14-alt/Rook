// per-conversation action log. lets the AI remember what it already clicked,
// what the prior tree looked like, so subsequent snapshots can be diffed.
//
// not persisted across backend restarts on purpose — the accessibility tree
// is ephemeral anyway. one restart, one fresh session.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionRecord {
    pub at: i64,        // unix epoch
    pub kind: String,   // "click" | "type" | "focus" | "snapshot"
    pub target: String, // element id or window title
    pub value: Option<String>,
    pub ok: bool,
    pub error: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ActionLog {
    pub actions: Vec<ActionRecord>,
    /// hashed summary of the last tree snapshot so we can detect when
    /// the window state changed between snapshots.
    pub last_tree_hash: Option<u64>,
    /// last serialized tree for diff purposes (capped at 64 KB).
    pub last_tree_json: Option<String>,
}

fn logs() -> &'static Mutex<HashMap<String, ActionLog>> {
    static LOGS: OnceLock<Mutex<HashMap<String, ActionLog>>> = OnceLock::new();
    LOGS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn record(conv_id: &str, rec: ActionRecord) {
    if let Ok(mut guard) = logs().lock() {
        let entry = guard.entry(conv_id.to_string()).or_default();
        entry.actions.push(rec);
        // keep the last 50 actions, drop older ones
        let len = entry.actions.len();
        if len > 50 {
            entry.actions.drain(0..(len - 50));
        }
    }
}

pub fn set_last_tree(conv_id: &str, tree_json: &str) {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    tree_json.hash(&mut h);
    let hash = h.finish();
    if let Ok(mut guard) = logs().lock() {
        let entry = guard.entry(conv_id.to_string()).or_default();
        entry.last_tree_hash = Some(hash);
        entry.last_tree_json = Some(tree_json.chars().take(65_536).collect());
    }
}

pub fn get(conv_id: &str) -> ActionLog {
    logs()
        .lock()
        .ok()
        .and_then(|g| g.get(conv_id).cloned())
        .unwrap_or_default()
}

pub fn compact_summary(conv_id: &str, last_n: usize) -> String {
    let log = get(conv_id);
    if log.actions.is_empty() {
        return String::new();
    }
    let start = log.actions.len().saturating_sub(last_n);
    log.actions[start..]
        .iter()
        .map(|a| {
            let status = if a.ok { "ok" } else { "fail" };
            match &a.value {
                Some(v) => format!(
                    "{} {} {} ({}) value={:?}",
                    a.kind, a.target, status, a.at, v
                ),
                None => format!("{} {} {} ({})", a.kind, a.target, status, a.at),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// DOM-style computer use for windows. Instead of screenshot-and-click,
// the AI gets a serialized accessibility tree of the foreground window
// and references elements by stable id. A per-conversation action log
// captures what's been done so the model can reason about prior steps
// without re-reading the whole tree.

#[cfg(windows)]
pub mod actions;
#[cfg(windows)]
pub mod uia;

pub mod context;

pub use context::ActionRecord;

#[cfg(not(windows))]
pub mod uia {
    use anyhow::{anyhow, Result};

    pub fn snapshot_foreground(_include_offscreen: bool) -> Result<serde_json::Value> {
        Err(anyhow!("computer use is windows-only in this build"))
    }
    pub fn snapshot_window(_title_substr: &str) -> Result<serde_json::Value> {
        Err(anyhow!("computer use is windows-only in this build"))
    }
    pub fn find_element(_query: &str) -> Result<serde_json::Value> {
        Err(anyhow!("computer use is windows-only in this build"))
    }
}

#[cfg(not(windows))]
pub mod actions {
    use anyhow::{anyhow, Result};

    pub fn click(_element_id: &str) -> Result<()> {
        Err(anyhow!("computer use is windows-only in this build"))
    }
    pub fn type_text(_element_id: &str, _text: &str) -> Result<()> {
        Err(anyhow!("computer use is windows-only in this build"))
    }
    pub fn focus_window(_title_substr: &str) -> Result<()> {
        Err(anyhow!("computer use is windows-only in this build"))
    }
}

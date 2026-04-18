// windows UI automation tree snapshot. we walk the accessibility tree of
// either the foreground window or a named window and serialize to a compact
// json form. elements get stable ids derived from their UIA runtime_id so
// the AI can reference them in subsequent click/type calls.
//
// COM objects aren't Send, so we never cache the IUIAutomationElement
// across threads. Instead we stash the uia runtime_id as a blob and
// re-resolve it on demand from the UIA root.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use uiautomation::{UIAutomation, UIElement};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiNode {
    pub id: String,
    pub role: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub bounds: [i32; 4],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shortcut: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invoke_hint: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub children: Vec<UiNode>,
}

thread_local! {
    // COM objects live on the thread that created them. We serve UIA calls
    // from the tool-executor threadpool, which is the same caller across
    // snapshot + action, so this registry works within one tool chain.
    // Falls back to re-snapshotting if it becomes stale.
    static REGISTRY: std::cell::RefCell<HashMap<String, UIElement>> =
        std::cell::RefCell::new(HashMap::new());
}

// id counter is Send-safe. resets on each snapshot.
fn next_counter() -> &'static Mutex<usize> {
    use std::sync::OnceLock;
    static C: OnceLock<Mutex<usize>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(0))
}

fn ua() -> Result<UIAutomation> {
    UIAutomation::new().context("init UIAutomation")
}

fn reset_registry() {
    REGISTRY.with(|r| r.borrow_mut().clear());
    if let Ok(mut c) = next_counter().lock() {
        *c = 0;
    }
}

fn stash(el: &UIElement) -> String {
    let id = {
        let mut c = next_counter().lock().unwrap();
        let v = *c;
        *c += 1;
        format!("e{}", v)
    };
    REGISTRY.with(|r| {
        r.borrow_mut().insert(id.clone(), el.clone());
    });
    id
}

pub fn element_for_id(id: &str) -> Option<UIElement> {
    REGISTRY.with(|r| r.borrow().get(id).cloned())
}

fn serialize(
    el: &UIElement,
    depth: usize,
    max_depth: usize,
    include_offscreen: bool,
    a: &UIAutomation,
) -> Option<UiNode> {
    let offscreen = el.is_offscreen().unwrap_or(false);
    if offscreen && !include_offscreen {
        return None;
    }

    let id = stash(el);
    let role = el
        .get_control_type()
        .map(|c| format!("{:?}", c))
        .unwrap_or_else(|_| "Unknown".into());
    let name = el.get_name().unwrap_or_default();

    let bounds = el
        .get_bounding_rectangle()
        .map(|r| [r.get_left(), r.get_top(), r.get_width(), r.get_height()])
        .unwrap_or([0, 0, 0, 0]);

    let value = el
        .get_pattern::<uiautomation::patterns::UIValuePattern>()
        .ok()
        .and_then(|p| p.get_value().ok())
        .filter(|v| !v.is_empty());

    let shortcut = el.get_accelerator_key().ok().filter(|s| !s.is_empty());

    let invoke_hint = if el
        .get_pattern::<uiautomation::patterns::UIInvokePattern>()
        .is_ok()
    {
        Some("click".to_string())
    } else if el
        .get_pattern::<uiautomation::patterns::UITogglePattern>()
        .is_ok()
    {
        Some("toggle".to_string())
    } else if el
        .get_pattern::<uiautomation::patterns::UISelectionItemPattern>()
        .is_ok()
    {
        Some("select".to_string())
    } else if el
        .get_pattern::<uiautomation::patterns::UIValuePattern>()
        .is_ok()
    {
        Some("edit".to_string())
    } else {
        None
    };

    let mut children = Vec::new();
    if depth < max_depth {
        if let Ok(walker) = a.get_control_view_walker() {
            let mut cur = walker.get_first_child(el).ok();
            while let Some(child) = cur {
                if let Some(node) = serialize(&child, depth + 1, max_depth, include_offscreen, a) {
                    children.push(node);
                }
                cur = walker.get_next_sibling(&child).ok();
            }
        }
    }

    Some(UiNode {
        id,
        role,
        name,
        value,
        bounds,
        shortcut,
        invoke_hint,
        children,
    })
}

pub fn snapshot_foreground(include_offscreen: bool) -> Result<serde_json::Value> {
    reset_registry();
    let a = ua()?;
    let hwnd = unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
    let root = a
        .element_from_handle(uiautomation::types::Handle::from(hwnd.0 as isize))
        .context("foreground HWND -> UIA element")?;
    let tree =
        serialize(&root, 0, 12, include_offscreen, &a).ok_or_else(|| anyhow!("empty tree"))?;
    Ok(serde_json::to_value(tree)?)
}

pub fn snapshot_window(title_substr: &str) -> Result<serde_json::Value> {
    reset_registry();
    let a = ua()?;
    let root = a.get_root_element().context("root element")?;
    let walker = a.get_control_view_walker().context("walker")?;

    let mut cur = walker.get_first_child(&root).ok();
    let needle = title_substr.to_ascii_lowercase();
    let target = loop {
        let Some(ref el) = cur else { break None };
        let name = el.get_name().unwrap_or_default().to_ascii_lowercase();
        if name.contains(&needle) {
            break Some(el.clone());
        }
        cur = walker.get_next_sibling(el).ok();
    };
    let target = target.ok_or_else(|| anyhow!("no window matching {:?}", title_substr))?;
    let tree = serialize(&target, 0, 12, false, &a).ok_or_else(|| anyhow!("empty tree"))?;
    Ok(serde_json::to_value(tree)?)
}

pub fn find_element(query: &str) -> Result<serde_json::Value> {
    let q = query.to_ascii_lowercase();
    REGISTRY.with(|r| {
        let reg = r.borrow();
        let hits: Vec<serde_json::Value> = reg
            .iter()
            .filter_map(|(id, el)| {
                let name = el.get_name().unwrap_or_default().to_ascii_lowercase();
                if name.contains(&q) {
                    Some(serde_json::json!({
                        "id": id,
                        "name": el.get_name().unwrap_or_default(),
                        "role": el.get_control_type().map(|c| format!("{:?}", c)).unwrap_or_default(),
                    }))
                } else {
                    None
                }
            })
            .take(10)
            .collect();
        Ok(serde_json::Value::Array(hits))
    })
}

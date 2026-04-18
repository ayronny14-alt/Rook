// execute an action against a previously-snapshotted element.

use anyhow::{anyhow, Result};
use uiautomation::inputs::Keyboard;
use uiautomation::patterns::{UIInvokePattern, UIValuePattern};

use super::uia::element_for_id;

pub fn click(element_id: &str) -> Result<()> {
    let el = element_for_id(element_id).ok_or_else(|| {
        anyhow!(
            "unknown element id {:?} — did you snapshot first?",
            element_id
        )
    })?;
    // prefer the invoke pattern (more reliable than synthetic mouse click);
    // fall back to el.click() which does a real cursor click.
    if let Ok(pat) = el.get_pattern::<UIInvokePattern>() {
        pat.invoke().map_err(|e| anyhow!("invoke failed: {}", e))?;
        return Ok(());
    }
    el.click().map_err(|e| anyhow!("click failed: {}", e))
}

pub fn type_text(element_id: &str, text: &str) -> Result<()> {
    let el = element_for_id(element_id).ok_or_else(|| {
        anyhow!(
            "unknown element id {:?} — did you snapshot first?",
            element_id
        )
    })?;
    // set_value via the value pattern is safer than key synthesis;
    // fall back to real typing if the element rejects it.
    if let Ok(pat) = el.get_pattern::<UIValuePattern>() {
        if pat.set_value(text).is_ok() {
            return Ok(());
        }
    }
    el.set_focus().ok();
    let kb = Keyboard::new();
    kb.send_text(text)
        .map_err(|e| anyhow!("type failed: {}", e))
}

pub fn focus_window(title_substr: &str) -> Result<()> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextW, SetForegroundWindow,
    };

    struct Acc<'a> {
        query: &'a str,
        hit: Option<HWND>,
    }

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let acc: &mut Acc = &mut *(lparam.0 as *mut Acc);
        let mut buf = [0u16; 512];
        let n = GetWindowTextW(hwnd, &mut buf);
        if n > 0 {
            let title = String::from_utf16_lossy(&buf[..n as usize]);
            if title
                .to_ascii_lowercase()
                .contains(&acc.query.to_ascii_lowercase())
            {
                acc.hit = Some(hwnd);
                return BOOL(0); // stop enumeration
            }
        }
        BOOL(1)
    }

    let mut acc = Acc {
        query: title_substr,
        hit: None,
    };
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut acc as *mut _ as isize));
        if let Some(hwnd) = acc.hit {
            let _ = SetForegroundWindow(hwnd);
            return Ok(());
        }
    }
    Err(anyhow!("no window matching {:?}", title_substr))
}

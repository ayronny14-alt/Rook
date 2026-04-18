use anyhow::Result;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;

use crate::tools::browser_automation::BrowserAutomation;
use crate::tools::browser_cdp::CdpBrowser;

pub struct BrowserTool {
    automation: Option<BrowserAutomation>,
    cdp: Option<CdpBrowser>,
}

impl BrowserTool {
    pub fn new() -> Self {
        Self {
            automation: None,
            cdp: None,
        }
    }

    pub fn spawn_automation(&mut self, headless: bool) -> Result<String> {
        let ba = BrowserAutomation::spawn_chrome(headless)?;
        let url = ba.debugging_url();
        self.automation = Some(ba);
        Ok(url)
    }

    pub fn spawn_cdp(&mut self, headless: bool) -> Result<String> {
        let b = CdpBrowser::new(headless)?;
        let url = b.debugging_url();
        self.cdp = Some(b);
        Ok(url)
    }

    pub fn navigate(&mut self, url: &str) -> Result<String> {
        if let Some(c) = self.cdp.as_mut() {
            c.navigate(url)
        } else {
            Err(anyhow::anyhow!("cdp browser not spawned"))
        }
    }

    pub fn click(&mut self, selector: &str) -> Result<()> {
        if let Some(c) = self.cdp.as_mut() {
            c.click(selector)
        } else {
            Err(anyhow::anyhow!("cdp browser not spawned"))
        }
    }

    pub fn type_str(&mut self, selector: &str, text: &str) -> Result<()> {
        if let Some(c) = self.cdp.as_mut() {
            c.type_str(selector, text)
        } else {
            Err(anyhow::anyhow!("cdp browser not spawned"))
        }
    }

    pub fn evaluate(&mut self, js: &str) -> Result<String> {
        if let Some(c) = self.cdp.as_mut() {
            c.evaluate(js)
        } else {
            Err(anyhow::anyhow!("cdp browser not spawned"))
        }
    }

    pub fn screenshot_base64(&mut self, full: bool) -> Result<String> {
        if let Some(c) = self.cdp.as_mut() {
            let bytes = c.screenshot(full)?;
            Ok(STANDARD.encode(&bytes))
        } else {
            Err(anyhow::anyhow!("cdp browser not spawned"))
        }
    }

    pub fn debugging_url(&self) -> Option<String> {
        self.cdp
            .as_ref()
            .map(|c| c.debugging_url())
            .or_else(|| self.automation.as_ref().map(|a| a.debugging_url()))
    }

    pub fn kill_all(&mut self) -> Result<()> {
        if let Some(a) = self.automation.as_mut() {
            let _ = a.kill();
        }
        if let Some(c) = self.cdp.take() {
            let _ = c.kill();
        }
        self.automation = None;
        Ok(())
    }
}

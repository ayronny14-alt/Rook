use anyhow::{anyhow, Result};
use headless_chrome::Tab;
use headless_chrome::{Browser, LaunchOptionsBuilder};
use std::sync::Arc;

pub struct CdpBrowser {
    _browser: Browser,
    tab: Arc<Tab>,
}

impl CdpBrowser {
    pub fn new(headless: bool) -> Result<Self> {
        let launch_opts = LaunchOptionsBuilder::default()
            .headless(headless)
            .build()
            .map_err(|e| anyhow!(e.to_string()))?;

        let browser = Browser::new(launch_opts).map_err(|e| anyhow!(e.to_string()))?;
        let tab = browser.new_tab().map_err(|e| anyhow!(e.to_string()))?;
        Ok(Self {
            _browser: browser,
            tab,
        })
    }

    pub fn debugging_url(&self) -> String {
        // headless_chrome doesn't expose CDP URL easily; return a generic local json endpoint
        "http://127.0.0.1:9222/json".to_string()
    }

    pub fn navigate(&mut self, url: &str) -> Result<String> {
        let nav = self
            .tab
            .navigate_to(url)
            .map_err(|e| anyhow!(e.to_string()))?;
        nav.wait_until_navigated()
            .map_err(|e| anyhow!(e.to_string()))?;
        let content = self.tab.get_content().map_err(|e| anyhow!(e.to_string()))?;
        Ok(content)
    }

    pub fn click(&mut self, selector: &str) -> Result<()> {
        let el = self
            .tab
            .wait_for_element(selector)
            .map_err(|e| anyhow!(e.to_string()))?;
        el.click().map_err(|e| anyhow!(e.to_string()))?;
        Ok(())
    }

    pub fn type_str(&mut self, selector: &str, text: &str) -> Result<()> {
        let el = self
            .tab
            .wait_for_element(selector)
            .map_err(|e| anyhow!(e.to_string()))?;
        el.click().map_err(|e| anyhow!(e.to_string()))?;
        el.type_into(text).map_err(|e| anyhow!(e.to_string()))?;
        Ok(())
    }

    pub fn evaluate(&mut self, js: &str) -> Result<String> {
        let v = self
            .tab
            .evaluate(js, false)
            .map_err(|e| anyhow!(e.to_string()))?;
        Ok(format!("{:?}", v.value))
    }

    pub fn screenshot(&mut self, _full: bool) -> Result<Vec<u8>> {
        let bytes = self
            .tab
            .capture_screenshot(
                headless_chrome::protocol::cdp::Page::CaptureScreenshotFormatOption::Png,
                None,
                None,
                true,
            )
            .map_err(|e| anyhow!(e.to_string()))?;

        Ok(bytes)
    }

    pub fn kill(self) -> Result<()> {
        // Drop browser to kill
        drop(self);
        Ok(())
    }
}

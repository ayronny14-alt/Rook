use anyhow::Result;
use reqwest::Client;
use scraper::{Html, Selector};
use tracing::debug;

pub struct WebSearchTool {
    http_client: Client,
}

impl WebSearchTool {
    pub fn new() -> Self {
        Self {
            http_client: Client::new(),
        }
    }

    pub async fn execute(&self, query: &str) -> Result<Vec<serde_json::Value>> {
        debug!("Web search: {}", query);

        let url = format!(
            "https://html.duckduckgo.com/html/?q={}",
            urlencoding::encode(query)
        );

        let html = self.http_client.get(&url).send().await?.text().await?;
        let document = Html::parse_document(&html);

        let result_selector = Selector::parse(".result__a").unwrap();
        let snippet_selector = Selector::parse(".result__snippet").unwrap();

        let mut results = Vec::new();

        for (i, element) in document.select(&result_selector).enumerate().take(10) {
            let title = element
                .text()
                .collect::<Vec<_>>()
                .join(" ")
                .trim()
                .to_string();
            let url = element.value().attr("href").unwrap_or("").to_string();

            let snippet = document
                .select(&snippet_selector)
                .nth(i)
                .map(|el| el.text().collect::<Vec<_>>().join(" ").trim().to_string())
                .unwrap_or_default();

            if title.is_empty() {
                continue;
            }
            results.push(serde_json::json!({
                "title": title,
                "url": url,
                "snippet": snippet,
            }));
        }

        // Fallback: try alternative selectors if DuckDuckGo changed its HTML structure
        if results.is_empty() {
            debug!("Primary DDG selectors returned no results; trying fallback selectors");
            let fallback_pairs: &[(&str, &str)] = &[
                ("a.result-link", ".result-snippet"),
                ("h2.result__title a", ".result__body"),
                (".results_links a.large", ".result__description"),
            ];
            'outer: for (title_sel_str, snip_sel_str) in fallback_pairs {
                if let (Ok(title_sel), Ok(snip_sel)) = (
                    Selector::parse(title_sel_str),
                    Selector::parse(snip_sel_str),
                ) {
                    for (i, element) in document.select(&title_sel).enumerate().take(10) {
                        let title = element
                            .text()
                            .collect::<Vec<_>>()
                            .join(" ")
                            .trim()
                            .to_string();
                        let url = element.value().attr("href").unwrap_or("").to_string();
                        let snippet = document
                            .select(&snip_sel)
                            .nth(i)
                            .map(|el| el.text().collect::<Vec<_>>().join(" ").trim().to_string())
                            .unwrap_or_default();
                        if title.is_empty() {
                            continue;
                        }
                        results.push(
                            serde_json::json!({ "title": title, "url": url, "snippet": snippet }),
                        );
                    }
                    if !results.is_empty() {
                        break 'outer;
                    }
                }
            }
        }

        if results.is_empty() {
            debug!("Web search returned no results for query: {}", query);
        }

        Ok(results)
    }

    pub async fn fetch_url(&self, url: &str) -> Result<String> {
        debug!("Fetching URL: {}", url);
        let html = self.http_client.get(url).send().await?.text().await?;
        let document = Html::parse_document(&html);

        let body_selector = Selector::parse("body").unwrap();
        let body_text = document
            .select(&body_selector)
            .next()
            .map(|el| el.text().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();

        Ok(body_text.chars().take(5000).collect())
    }
}

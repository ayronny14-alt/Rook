// Discovers the cheapest model available at an OpenAI-compatible /models endpoint.
// Used to route auxiliary work (distillation, auto-title, intent classification)
// away from the user's primary paid model.

use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

// ordered from cheapest → most expensive on common providers.
// first keyword that matches a model id wins. case-insensitive substring match.
const CHEAP_HINTS: &[&str] = &[
    "nova-fast",
    "openai-fast",
    "haiku",
    "nano",
    "mini",
    "flash-lite",
    "flash",
    "small",
    "fast",
    "turbo",
    "8b",
    "7b",
    "3b",
];

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
}

#[derive(Clone)]
struct CachedPick {
    model: Option<String>,
    at: Instant,
}

// base_url -> (model, cached_at). 1-hour TTL is plenty — model catalogues rarely change.
fn cache() -> &'static Mutex<HashMap<String, CachedPick>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CachedPick>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

const CACHE_TTL: Duration = Duration::from_secs(60 * 60);

/// Returns the cheapest-looking model id served by the provider, or `None`
/// if the endpoint doesn't expose one. Result is cached for an hour per base_url.
pub async fn cheapest_model(http: &Client, base_url: &str, api_key: &str) -> Option<String> {
    let key = base_url.trim_end_matches('/').to_string();

    // Fast path: cache hit
    if let Ok(guard) = cache().lock() {
        if let Some(hit) = guard.get(&key) {
            if hit.at.elapsed() < CACHE_TTL {
                return hit.model.clone();
            }
        }
    }

    let url = format!("{}/models", key);
    let mut req = http.get(&url).timeout(Duration::from_secs(5));
    if !api_key.trim().is_empty() {
        req = req.header("Authorization", format!("Bearer {}", api_key));
    }
    let pick = match req.send().await {
        Ok(r) if r.status().is_success() => r
            .json::<ModelsResponse>()
            .await
            .ok()
            .and_then(|m| rank_cheapest(&m.data)),
        _ => None,
    };

    if let Ok(mut guard) = cache().lock() {
        guard.insert(
            key,
            CachedPick {
                model: pick.clone(),
                at: Instant::now(),
            },
        );
    }
    pick
}

fn rank_cheapest(entries: &[ModelEntry]) -> Option<String> {
    let ids: Vec<String> = entries.iter().map(|e| e.id.to_ascii_lowercase()).collect();
    for hint in CHEAP_HINTS {
        if let Some(idx) = ids.iter().position(|id| id.contains(hint)) {
            return Some(entries[idx].id.clone());
        }
    }
    // Nothing matched — don't route. Caller falls back to primary.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_haiku_over_sonnet() {
        let entries = vec![
            ModelEntry {
                id: "claude-3-5-sonnet".into(),
            },
            ModelEntry {
                id: "claude-3-5-haiku".into(),
            },
            ModelEntry {
                id: "claude-3-opus".into(),
            },
        ];
        assert_eq!(rank_cheapest(&entries).as_deref(), Some("claude-3-5-haiku"));
    }

    #[test]
    fn picks_mini_over_turbo() {
        let entries = vec![
            ModelEntry {
                id: "gpt-4-turbo".into(),
            },
            ModelEntry {
                id: "gpt-4o-mini".into(),
            },
            ModelEntry {
                id: "gpt-4o".into(),
            },
        ];
        assert_eq!(rank_cheapest(&entries).as_deref(), Some("gpt-4o-mini"));
    }

    #[test]
    fn none_when_nothing_matches() {
        let entries = vec![ModelEntry {
            id: "some-premium-model".into(),
        }];
        assert!(rank_cheapest(&entries).is_none());
    }
}

// GitHub search client for Rook plugins.
use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{debug, warn};

use super::registry::{plugin_from_github, Plugin, PluginType};

// Canonical topic lists

const MCP_TOPICS: &[&str] = &[
    "mcp-server",
    "model-context-protocol",
    "modelcontextprotocol",
];

const SKILL_TOPICS: &[&str] = &[
    "rook-skill",
    "agent-skill",
    // Ecosystem-compatible topics - these repos work with Rook too.
    "claude-skill",
    "anthropic-skill",
];

const CONNECTOR_TOPICS: &[&str] = &[
    "rook-connector",
    // Ecosystem-compatible topics - these connectors are protocol-compatible.
    "claude-connector",
    "anthropic-connector",
];

fn canonical_topics(ptype: &PluginType) -> &'static [&'static str] {
    match ptype {
        PluginType::Mcp => MCP_TOPICS,
        PluginType::Skill => SKILL_TOPICS,
        PluginType::Connector => CONNECTOR_TOPICS,
    }
}

// GitHub API response shapes

#[derive(Debug, Deserialize)]
struct SearchResponse {
    items: Vec<RepoItem>,
}

#[derive(Debug, Deserialize)]
struct RepoItem {
    name: String,
    full_name: String,
    html_url: String,
    description: Option<String>,
    stargazers_count: i64,
    owner: OwnerItem,
    default_branch: Option<String>,
    topics: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct OwnerItem {
    login: String,
}

#[derive(Debug, Deserialize)]
struct ContentResponse {
    content: Option<String>, // base64-encoded
}

// Public surface

// Performs GitHub repository searches across the canonical topics for each
pub async fn search_github(query: &str, token: Option<&str>) -> Result<Vec<Plugin>> {
    let client = build_client(token)?;
    let trimmed = query.trim();
    let mut results: Vec<Plugin> = Vec::new();

    // Run one search per canonical topic across all plugin types. This yields
    // between 7 and 10 search requests - still inside GitHub's 10/min
    // unauth'd rate limit for a single user action, and well inside the
    // 30/min authenticated limit.
    let types = [PluginType::Mcp, PluginType::Skill, PluginType::Connector];

    for ptype in types.iter() {
        let topics = canonical_topics(ptype);
        for topic in topics {
            let q = if trimmed.is_empty() {
                format!("topic:{}", topic)
            } else {
                format!("{} topic:{}", trimmed, topic)
            };
            match search_repos(&client, &q, ptype.clone(), topics).await {
                Ok(mut items) => results.append(&mut items),
                Err(e) => warn!("GitHub search '{}' failed: {}", q, e),
            }
        }
    }

    // Sort by stars (highest first), then dedupe by repo URL (a single repo
    // can legitimately match more than one canonical topic).
    results.sort_by_key(|r| std::cmp::Reverse(r.stars));
    results.dedup_by(|a, b| a.repo_url == b.repo_url);
    Ok(results)
}

/// Fetch the README (or package.json / pyproject.toml) for a repo and return it
/// as plain text.  Useful for the AI to understand what a plugin does before recommending.
pub async fn fetch_repo_readme(owner: &str, repo: &str, token: Option<&str>) -> Result<String> {
    let client = build_client(token)?;
    let url = format!("https://api.github.com/repos/{}/{}/readme", owner, repo);
    let resp: ContentResponse = client
        .get(&url)
        .send()
        .await
        .context("README fetch")?
        .json()
        .await
        .context("README parse")?;

    if let Some(b64) = resp.content {
        let clean: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
        let bytes = base64_decode(&clean).unwrap_or_default();
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    } else {
        Ok(String::new())
    }
}

/// Detect the primary language / entry-point hints from the repo root file listing.
pub async fn fetch_repo_tree(
    owner: &str,
    repo: &str,
    branch: &str,
    token: Option<&str>,
) -> Result<Vec<String>> {
    let client = build_client(token)?;
    let url = format!(
        "https://api.github.com/repos/{}/{}/git/trees/{}?recursive=0",
        owner, repo, branch
    );

    #[derive(Deserialize)]
    struct TreeResp {
        tree: Vec<TreeEntry>,
    }
    #[derive(Deserialize)]
    struct TreeEntry {
        path: String,
    }

    let resp: TreeResp = client.get(&url).send().await?.json().await?;
    Ok(resp.tree.into_iter().map(|e| e.path).collect())
}

// Private helpers

fn build_client(token: Option<&str>) -> Result<reqwest::Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::USER_AGENT,
        reqwest::header::HeaderValue::from_static("Rook/1.0"),
    );
    headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static("application/vnd.github+json"),
    );
    if let Some(tok) = token {
        let val = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", tok))
            .context("invalid token")?;
        headers.insert(reqwest::header::AUTHORIZATION, val);
    }
    Ok(reqwest::Client::builder()
        .default_headers(headers)
        .timeout(std::time::Duration::from_secs(15))
        .build()?)
}

async fn search_repos(
    client: &reqwest::Client,
    query: &str,
    plugin_type: PluginType,
    accept_topics: &[&str],
) -> Result<Vec<Plugin>> {
    let url = format!(
        "https://api.github.com/search/repositories?q={}&sort=stars&order=desc&per_page=15",
        urlencoding::encode(query)
    );
    debug!("GitHub search: {}", url);

    let resp = client.get(&url).send().await.context("GitHub request")?;

    if !resp.status().is_success() {
        warn!("GitHub API {} for '{}'", resp.status(), query);
        return Ok(vec![]);
    }

    let body: SearchResponse = resp.json().await.context("GitHub JSON parse")?;
    let mut out = Vec::new();
    for item in body.items {
        // Strict filter
        let topics = item.topics.as_deref().unwrap_or(&[]);
        let has_canonical = topics.iter().any(|t| {
            let t = t.to_ascii_lowercase();
            accept_topics.iter().any(|canon| t == *canon)
        });
        if !has_canonical {
            debug!(
                "drop '{}': topics={:?} (no canonical match for {:?})",
                item.full_name, topics, plugin_type
            );
            continue;
        }

        let (owner_str, repo_str) = {
            let parts: Vec<&str> = item.full_name.splitn(2, '/').collect();
            if parts.len() == 2 {
                (parts[0].to_string(), parts[1].to_string())
            } else {
                (item.owner.login.clone(), item.name.clone())
            }
        };

        out.push(plugin_from_github(
            &item.name,
            item.description,
            plugin_type.clone(),
            &item.html_url,
            &owner_str,
            &repo_str,
            item.stargazers_count,
        ));
    }
    Ok(out)
}

/// Minimal base64 decode (avoids adding a heavy dependency - re-uses the
/// base64 crate already present in Cargo.toml).
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

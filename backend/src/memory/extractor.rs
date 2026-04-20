// Deterministic, code-based fact extractor. Runs synchronously on every
// chat turn with zero LLM calls. Targets the signals that actually matter:
// user self-identity, project/product names, people, tools, places. Dedup
// is aggressive - case-insensitive concept titles, ON CONFLICT user_facts.
//
// Not clever like an LLM. Clever like a regex that never hallucinates.

use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use tracing::info;

// Tiny helper so the rest of the file reads like once_cell::Lazy.
struct Lazy<T: 'static, F = fn() -> T> {
    cell: OnceLock<T>,
    init: F,
}
impl<T: 'static, F: Fn() -> T> Lazy<T, F> {
    const fn new(init: F) -> Self {
        Self {
            cell: OnceLock::new(),
            init,
        }
    }
    fn get(&self) -> &T {
        self.cell.get_or_init(&self.init)
    }
}
impl<T: 'static, F: Fn() -> T> std::ops::Deref for Lazy<T, F> {
    type Target = T;
    fn deref(&self) -> &T {
        self.get()
    }
}

use crate::memory::graph::{GraphMemory, NodeType};
use crate::memory::storage::MemoryStorage;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EntityKind {
    Person,
    Project,
    Organization,
    Place,
    Tool,
    Url,
    Path,
}

impl EntityKind {
    fn tag(&self) -> &'static str {
        match self {
            EntityKind::Person => "person",
            EntityKind::Project => "project",
            EntityKind::Organization => "organization",
            EntityKind::Place => "place",
            EntityKind::Tool => "tool",
            EntityKind::Url => "url",
            EntityKind::Path => "path",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Extracted {
    pub user_facts: HashMap<String, String>,
    pub entities: Vec<(String, EntityKind)>,
}

// Common English/chat words that regex capitalization tricks will also
// match. Keep them out of the entity stream.
static STOPWORDS: Lazy<HashSet<&'static str>, fn() -> HashSet<&'static str>> = Lazy::new(|| {
    [
        "I",
        "I'm",
        "I've",
        "I'll",
        "I'd",
        "You",
        "We",
        "They",
        "He",
        "She",
        "It",
        "The",
        "A",
        "An",
        "And",
        "Or",
        "But",
        "So",
        "If",
        "Then",
        "That",
        "This",
        "These",
        "Those",
        "My",
        "Your",
        "His",
        "Her",
        "Our",
        "Their",
        "Its",
        "Me",
        "Us",
        "Them",
        "Him",
        "Yes",
        "No",
        "Ok",
        "Okay",
        "Sure",
        "Maybe",
        "Please",
        "Thanks",
        "Thank",
        "What",
        "Why",
        "How",
        "When",
        "Where",
        "Who",
        "Which",
        "Today",
        "Tomorrow",
        "Yesterday",
        "Now",
        "Later",
        "Soon",
        "Never",
        "Always",
        "Can",
        "Could",
        "Would",
        "Should",
        "Will",
        "Shall",
        "May",
        "Might",
        "Must",
        "Do",
        "Does",
        "Did",
        "Have",
        "Has",
        "Had",
        "Be",
        "Been",
        "Being",
        "Is",
        "Are",
        "Was",
        "Were",
        "Hello",
        "Hi",
        "Hey",
        "Bye",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ]
    .into_iter()
    .collect()
});

// Hardcoded lexicon of tools/tech we recognize on sight. Extend freely.
static TOOL_LEXICON: Lazy<HashSet<&'static str>, fn() -> HashSet<&'static str>> = Lazy::new(|| {
    [
        "rust",
        "cargo",
        "tokio",
        "electron",
        "react",
        "vue",
        "svelte",
        "angular",
        "node",
        "npm",
        "pnpm",
        "yarn",
        "bun",
        "deno",
        "typescript",
        "javascript",
        "python",
        "pip",
        "poetry",
        "uv",
        "conda",
        "django",
        "flask",
        "fastapi",
        "go",
        "golang",
        "java",
        "kotlin",
        "swift",
        "c++",
        "c#",
        "ruby",
        "rails",
        "postgres",
        "postgresql",
        "sqlite",
        "mysql",
        "redis",
        "mongodb",
        "dynamodb",
        "kafka",
        "rabbitmq",
        "nats",
        "docker",
        "kubernetes",
        "k8s",
        "terraform",
        "ansible",
        "helm",
        "aws",
        "gcp",
        "azure",
        "vercel",
        "netlify",
        "cloudflare",
        "fly.io",
        "git",
        "github",
        "gitlab",
        "bitbucket",
        "vscode",
        "vim",
        "neovim",
        "emacs",
        "sublime",
        "jetbrains",
        "intellij",
        "webstorm",
        "cursor",
        "windows",
        "macos",
        "linux",
        "ubuntu",
        "debian",
        "arch",
        "fedora",
        "slack",
        "discord",
        "notion",
        "linear",
        "jira",
        "figma",
        "openai",
        "anthropic",
        "ollama",
        "huggingface",
        "sqlite",
        "rusqlite",
        "serde",
        "clap",
        "axum",
        "actix",
    ]
    .into_iter()
    .collect()
});

// PascalCase / multi-word Capitalized name candidate.
// Requires at least one uppercase letter followed by lower-case, to skip
// ACRONYMS (we handle those separately) and single-letter matches.
static RE_PROPER: Lazy<Regex, fn() -> Regex> =
    Lazy::new(|| Regex::new(r"\b([A-Z][a-z]+(?:[A-Z][a-z]+|\s+[A-Z][a-z]+){0,3})\b").unwrap());

// owner/repo style. Limited chars so we don't pick up every slash.
static RE_REPO: Lazy<Regex, fn() -> Regex> =
    Lazy::new(|| Regex::new(r"\b([A-Za-z0-9][\w.-]{1,38})/([A-Za-z0-9][\w.-]{1,99})\b").unwrap());

static RE_URL: Lazy<Regex, fn() -> Regex> =
    Lazy::new(|| Regex::new(r"https?://[^\s<>\)\]]+").unwrap());

// Unix-ish or windows paths. We only grab paths with a separator so we
// don't confuse every filename.
static RE_PATH: Lazy<Regex, fn() -> Regex> = Lazy::new(|| {
    Regex::new(r"(?:[A-Za-z]:[\\/][\w\\/.-]+|(?:~|\.{1,2})?/[\w./-]+/[\w./-]+)").unwrap()
});

// Self-identity patterns. Each pattern emits a (key, capture-group index).
struct IdPattern {
    key: &'static str,
    re: Regex,
}

static SELF_PATTERNS: Lazy<Vec<IdPattern>, fn() -> Vec<IdPattern>> = Lazy::new(|| {
    // (?i) on the lead-in, (?-i:...) on the capture so we keep proper-case
    // detection working (otherwise "aaron fricker and i work" all match).
    let raw: &[(&str, &str)] = &[
        (
            "name",
            r"(?i)\b(?:my\s+name\s+is|i'?m\s+called|call\s+me|i\s+am)\s+(?-i:([A-Z][A-Za-z'-]{1,30}(?:\s+[A-Z][A-Za-z'-]{1,30}){0,2}))",
        ),
        (
            "name",
            r"(?i)^\s*i'?m\s+(?-i:([A-Z][A-Za-z'-]{1,30}(?:\s+[A-Z][A-Za-z'-]{1,30})?))\b",
        ),
        (
            "company",
            r"(?i)\bi\s+(?:work|am)\s+(?:at|for)\s+(?-i:([A-Z][\w&.'-]{1,40}(?:\s+[A-Z][\w&.'-]{1,40}){0,3}))",
        ),
        (
            "location",
            r"(?i)\bi\s+(?:live|am|work)\s+in\s+(?-i:([A-Z][A-Za-z.'-]{1,40}(?:,?\s+[A-Z][A-Za-z.'-]{1,40}){0,2}))",
        ),
        (
            "location",
            r"(?i)\bi'?m\s+from\s+(?-i:([A-Z][A-Za-z.'-]{1,40}(?:,?\s+[A-Z][A-Za-z.'-]{1,40}){0,2}))",
        ),
        (
            "role",
            r"(?i)\bi'?m\s+a\s+([a-z][a-z\s-]{3,40}?)(?:\s+(?:at|for|who|and|\.|,))",
        ),
        (
            "editor",
            r"(?i)\bi\s+use\s+(vscode|vim|neovim|emacs|sublime|jetbrains|intellij|webstorm|cursor)\b",
        ),
        (
            "os",
            r"(?i)\bi'?m\s+on\s+(windows|macos|mac|linux|ubuntu|debian|arch|fedora)\b",
        ),
        (
            "language_primary",
            r"(?i)\bi\s+(?:mostly\s+)?(?:code|write|program)\s+(?:in|with)\s+([a-z+#]+)\b",
        ),
        (
            "timezone",
            r"\b(UTC[+\-]\d{1,2}|[A-Z]{2,4}T|Pacific|Eastern|Central|Mountain|GMT)\b",
        ),
    ];
    raw.iter()
        .filter_map(|(k, r)| Regex::new(r).ok().map(|re| IdPattern { key: k, re }))
        .collect()
});

// Explicit project-name signals: "project Foo", "codenamed Foo", quoted
// strings following "building"/"working on".
static RE_PROJECT_EXPLICIT: Lazy<Regex, fn() -> Regex> = Lazy::new(|| {
    Regex::new(
        r#"(?i)(?:project|codenamed|codename|building|working\s+on|shipping|called)\s+["']?([A-Z][\w.-]{1,60}(?:\s+[A-Z][\w.-]{1,40}){0,2})["']?"#,
    )
    .unwrap()
});

pub fn extract(text: &str) -> Extracted {
    // regex goes brrr. no tokens harmed.
    let mut user_facts: HashMap<String, String> = HashMap::new();
    let mut entities: Vec<(String, EntityKind)> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();

    for p in SELF_PATTERNS.iter() {
        if let Some(c) = p.re.captures(text) {
            if let Some(m) = c.get(1) {
                let mut v = m
                    .as_str()
                    .trim()
                    .trim_end_matches(['.', ',', '!', '?'])
                    .to_string();
                // tool-ish keys are conventionally lowercase
                if matches!(p.key, "editor" | "os" | "language_primary") {
                    v = v.to_ascii_lowercase();
                }
                if !v.is_empty() && v.len() <= 120 {
                    user_facts.entry(p.key.to_string()).or_insert(v);
                }
            }
        }
    }

    let lower = text.to_ascii_lowercase();
    for tool in TOOL_LEXICON.iter() {
        if word_match(&lower, tool) {
            push(&mut entities, &mut seen, tool, EntityKind::Tool);
        }
    }

    for c in RE_URL.captures_iter(text) {
        if let Some(m) = c.get(0) {
            push(&mut entities, &mut seen, m.as_str(), EntityKind::Url);
        }
    }

    for c in RE_PATH.captures_iter(text) {
        if let Some(m) = c.get(0) {
            let s = m.as_str();
            if s.len() >= 6 {
                push(&mut entities, &mut seen, s, EntityKind::Path);
            }
        }
    }

    for c in RE_REPO.captures_iter(text) {
        if let Some(m) = c.get(0) {
            let s = m.as_str();
            if !s.contains("://") && !s.starts_with('/') {
                push(&mut entities, &mut seen, s, EntityKind::Project);
            }
        }
    }

    for c in RE_PROJECT_EXPLICIT.captures_iter(text) {
        if let Some(m) = c.get(1) {
            let s = m.as_str().trim();
            if !STOPWORDS.contains(s) {
                push(&mut entities, &mut seen, s, EntityKind::Project);
            }
        }
    }

    // Proper-noun sweep. We mark every capitalized phrase as a Person
    // candidate if it's two+ words, else Organization. Cheap and good
    // enough; the graph dedupe keeps it honest.
    for c in RE_PROPER.captures_iter(text) {
        if let Some(m) = c.get(1) {
            let s = m.as_str().trim();
            if s.len() < 3 || s.len() > 80 {
                continue;
            }
            if STOPWORDS.contains(s) {
                continue;
            }
            // skip if first word is a stopword (sentence-start false positive)
            let first = s.split_whitespace().next().unwrap_or("");
            if STOPWORDS.contains(first) {
                continue;
            }
            // lexicon hits are already captured as tools
            if TOOL_LEXICON.contains(s.to_ascii_lowercase().as_str()) {
                continue;
            }
            let kind = if s.contains(' ') {
                EntityKind::Person
            } else {
                EntityKind::Organization
            };
            push(&mut entities, &mut seen, s, kind);
        }
    }

    Extracted {
        user_facts,
        entities,
    }
}

fn push(
    out: &mut Vec<(String, EntityKind)>,
    seen: &mut HashSet<(String, String)>,
    raw: &str,
    kind: EntityKind,
) {
    let norm = raw.trim().to_string();
    if norm.is_empty() {
        return;
    }
    let key = (norm.to_ascii_lowercase(), kind.tag().to_string());
    if seen.insert(key) {
        out.push((norm, kind));
    }
}

fn word_match(haystack_lower: &str, needle: &str) -> bool {
    // whole-word match against pre-lowercased haystack
    let bytes = haystack_lower.as_bytes();
    let n = needle.as_bytes();
    let mut i = 0;
    while let Some(pos) = haystack_lower[i..].find(needle) {
        let start = i + pos;
        let end = start + n.len();
        let before_ok = start == 0 || !is_word(bytes[start - 1]);
        let after_ok = end == bytes.len() || !is_word(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        i = start + 1;
        if i >= bytes.len() {
            break;
        }
    }
    false
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

// Persist directly. No LLM, no async. Called fire-and-forget from the
// chat handler so a slow sqlite transaction can't block the turn.
pub fn persist(memory: &MemoryStorage, ex: &Extracted) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    if !ex.user_facts.is_empty() {
        if let Ok(conn) = memory.get_connection() {
            for (k, v) in ex.user_facts.iter() {
                let _ = conn.execute(
                    "INSERT INTO user_facts (key, value, created_at, updated_at) VALUES (?1, ?2, ?3, ?3) \
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                    rusqlite::params![k, v, now],
                );
            }
            info!("Extractor: stored {} user fact(s)", ex.user_facts.len());
        }
    }

    if ex.entities.is_empty() {
        return;
    }
    let graph = GraphMemory::new(memory.clone());
    let mut created = 0usize;
    for (name, kind) in ex.entities.iter() {
        // case-insensitive title dedupe
        let existing = graph
            .search_nodes(Some(NodeType::Concept), Some(name))
            .unwrap_or_default()
            .into_iter()
            .any(|n| n.title.to_lowercase() == name.to_lowercase());
        if existing {
            continue;
        }
        let meta = serde_json::json!({
            "kind": kind.tag(),
            "source": "extractor",
            "confidence": 0.6,
        });
        if graph
            .create_node(NodeType::Concept, name, Some(meta))
            .is_ok()
        {
            created += 1;
        }
    }
    if created > 0 {
        info!("Extractor: created {} entity node(s)", created);
    }
}

pub fn run(memory: &MemoryStorage, user_message: &str, assistant_content: &str) {
    // combine both sides so entities the assistant names also get captured
    let combined = format!("{}\n{}", user_message, assistant_content);
    let ex = extract(&combined);
    if ex.user_facts.is_empty() && ex.entities.is_empty() {
        return;
    }
    persist(memory, &ex);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_name_and_company() {
        let ex = extract("Hey, my name is Aaron Fricker and I work at Anthropic.");
        assert_eq!(
            ex.user_facts.get("name").map(String::as_str),
            Some("Aaron Fricker")
        );
        assert!(ex.user_facts.get("company").is_some());
    }

    #[test]
    fn extracts_tools_and_location() {
        let ex = extract("I'm on Windows and I use vscode. I live in Seattle.");
        assert_eq!(ex.user_facts.get("os").map(String::as_str), Some("windows"));
        assert_eq!(
            ex.user_facts.get("editor").map(String::as_str),
            Some("vscode")
        );
        assert!(ex
            .entities
            .iter()
            .any(|(_, k)| matches!(k, EntityKind::Tool)));
    }

    #[test]
    fn explicit_project_name() {
        let ex = extract("I'm building Rook, a desktop AI app.");
        assert!(ex
            .entities
            .iter()
            .any(|(n, k)| matches!(k, EntityKind::Project) && n == "Rook"));
    }

    #[test]
    fn dedupes_and_ignores_stopwords() {
        let ex = extract("The quick brown fox. I think Rust is great.");
        // "The" and "I" should not be entities
        assert!(!ex.entities.iter().any(|(n, _)| n == "The" || n == "I"));
    }

    #[test]
    fn captures_repo_and_url() {
        let ex = extract("check ayronny14-alt/Rook and https://github.com/foo/bar");
        assert!(ex
            .entities
            .iter()
            .any(|(_, k)| matches!(k, EntityKind::Url)));
        assert!(ex
            .entities
            .iter()
            .any(|(_, k)| matches!(k, EntityKind::Project)));
    }
}

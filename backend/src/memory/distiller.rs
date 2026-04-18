// Background fact distiller. Runs after each chat turn, extracts long-term
// facts, dispatches to the cheapest model the provider serves.
use tracing::{info, warn};
use uuid::Uuid;

use crate::llm::client::LLMClient;
use crate::llm::types::Message;
use crate::memory::storage::MemoryStorage;

const DISTILL_TIMEOUT_SECS: u64 = 20;

/// Skip distillation when the exchange is unlikely to contain anything worth
/// extracting. Saves an LLM call on ~30-40% of turns.
fn is_low_signal(user_message: &str, assistant_content: &str) -> bool {
    let u = user_message.trim();
    let a = assistant_content.trim();
    if u.len() < 40 && a.len() < 150 {
        return true;
    }
    // pure acknowledgments
    let lower = u.to_ascii_lowercase();
    let ack = [
        "ok",
        "thanks",
        "thank you",
        "cool",
        "nice",
        "great",
        "yep",
        "yes",
        "no",
        "k",
        "kk",
    ];
    if ack
        .iter()
        .any(|w| lower == *w || lower == format!("{}.", w) || lower == format!("{}!", w))
    {
        return true;
    }
    false
}

/// Spawn a non-blocking background task that distils facts from the latest
/// user ↔ assistant exchange. Returns immediately.
pub fn spawn_distill(
    llm: LLMClient,
    memory: MemoryStorage,
    user_message: String,
    assistant_content: String,
) {
    if user_message.trim().is_empty() || assistant_content.trim().is_empty() {
        return;
    }
    if is_low_signal(&user_message, &assistant_content) {
        return;
    }
    tokio::spawn(async move {
        if let Err(_elapsed) = tokio::time::timeout(
            std::time::Duration::from_secs(DISTILL_TIMEOUT_SECS),
            run_distill(&llm, &memory, &user_message, &assistant_content),
        )
        .await
        {
            warn!("Distiller: timed out after {}s", DISTILL_TIMEOUT_SECS);
        }
    });
}

async fn run_distill(
    llm: &LLMClient,
    memory: &MemoryStorage,
    user_message: &str,
    assistant_content: &str,
) {
    // Truncate inputs so the prompt stays small (Nova Micro is fast but tiny)
    let user_trunc: String = user_message.chars().take(1_000).collect();
    let asst_trunc: String = assistant_content.chars().take(1_200).collect();

    let prompt = format!(
        r#"Extract long-term facts from this conversation exchange.
Return ONLY valid JSON — no markdown, no explanation:
{{
  "user_facts": [{{"key": "...", "value": "..."}}],
  "memory_facts": [{{"title": "...", "content": "...", "category": "..."}}]
}}

RULES:
- user_facts: ANY personal info about the user, including:
  name, age, job title, company, OS, editor/IDE, timezone, location, spoken language,
  food preferences, dietary restrictions, hobbies, interests, sports, pets, favourite
  music/movies/games, recurring tools, deployment targets, team structure, project name,
  productivity habits, anything the user says they like, dislike, prefer, or use regularly.
- memory_facts: technical or project knowledge — decisions made, bugs found/fixed, config
  values, architecture facts, API details, patterns established, discovered constraints,
  commands that work, gotchas learned.
- Only extract things explicitly stated. Do NOT infer or assume.
- Skip conversational filler, greetings, or single-word acknowledgements.
- Limit: up to 3 user_facts, up to 3 memory_facts per call.
- category for memory_facts: project | decision | architecture | bug | preference | api | config | pattern
- If nothing is worth storing, return {{"user_facts":[],"memory_facts":[]}}.

EXCHANGE:
User: {user}
Assistant: {assistant}"#,
        user = user_trunc,
        assistant = asst_trunc,
    );

    let messages = vec![Message::text("user", prompt)];

    // Pick the cheapest model the provider serves. Falls back to primary model
    // if the /models endpoint doesn't advertise a cheaper tier.
    let cheap = llm.cheapest_model().await;
    let response = match llm.chat_with_model_override(messages, &cheap, 512).await {
        Ok(r) => r,
        Err(e) => {
            if !e.to_string().contains("Not signed in") {
                warn!("Distiller: LLM call failed ({}) — {}", cheap, e);
            }
            return;
        }
    };

    let raw = response
        .choices
        .first()
        .and_then(|c| c.message.text_content())
        .unwrap_or_default();

    if raw.trim().is_empty() {
        return;
    }

    // Strip markdown code fences if the model wrapped the JSON
    let json_str = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    let parsed: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => {
            warn!(
                "Distiller: JSON parse failed — {} | raw snippet: {}",
                e,
                &raw[..raw.len().min(300)]
            );
            return;
        }
    };

    persist_user_facts(memory, &parsed);
    persist_memory_facts(llm, memory, &parsed).await;
}

fn persist_user_facts(memory: &MemoryStorage, parsed: &serde_json::Value) {
    let arr = match parsed.get("user_facts").and_then(|v| v.as_array()) {
        Some(a) if !a.is_empty() => a,
        _ => return,
    };

    let conn = match memory.get_connection() {
        Ok(c) => c,
        Err(e) => {
            warn!("Distiller: DB connection failed — {}", e);
            return;
        }
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let mut extracted = 0usize;
    for item in arr.iter().take(3) {
        let key = item
            .get("key")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let value = item
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if key.is_empty() || value.is_empty() || key.len() > 128 || value.len() > 512 {
            continue;
        }

        // Detect conflicts: check if the key already exists with a different value.
        // Archive the old value to user_fact_history so drift is traceable.
        let existing: std::result::Result<String, _> = conn.query_row(
            "SELECT value FROM user_facts WHERE key = ?1",
            rusqlite::params![key],
            |r| r.get::<_, String>(0),
        );
        if let Ok(old_val) = existing {
            if old_val != value {
                let preview_old = &old_val[..old_val.len().min(80)];
                let preview_new = &value[..value.len().min(80)];
                warn!(
                    "Distiller: user_fact '{}' changed: '{}' → '{}'",
                    key, preview_old, preview_new
                );
                let _ = conn.execute(
                    "INSERT INTO user_fact_history (id, key, old_value, new_value, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![Uuid::new_v4().to_string(), &key, &old_val, &value, now],
                );
            }
        }

        let _ = conn.execute(
            "INSERT INTO user_facts (key, value, created_at, updated_at) VALUES (?1, ?2, ?3, ?3) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            rusqlite::params![key, value, now],
        );
        extracted += 1;
    }
    if extracted > 0 {
        info!("Distiller: stored {} user fact(s)", extracted);
    }
}

async fn persist_memory_facts(llm: &LLMClient, memory: &MemoryStorage, parsed: &serde_json::Value) {
    let arr = match parsed.get("memory_facts").and_then(|v| v.as_array()) {
        Some(a) if !a.is_empty() => a,
        _ => return,
    };

    let graph = crate::memory::graph::GraphMemory::new(memory.clone());
    let obj_store = crate::memory::object::ObjectMemoryStore::new(memory.clone());
    let emb_store = crate::memory::embedding::EmbeddingMemory::new(memory.clone());

    // Phase 1: resolve node ids + collect embed-texts for a single batched call.
    let mut pending: Vec<(crate::memory::graph::Node, String, String, String)> = Vec::new();
    for item in arr.iter().take(3) {
        let title = item
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let content = item
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let category = item
            .get("category")
            .and_then(|v| v.as_str())
            .unwrap_or("general")
            .trim()
            .to_string();

        if title.is_empty() || content.is_empty() || title.len() > 200 || content.len() > 2_000 {
            continue;
        }

        let meta = serde_json::json!({
            "category": category,
            "source": "distiller",
            "confidence": 0.75,
        });

        // Deduplicate: if a Concept node with this title already exists, reuse it
        // rather than fragmenting the graph with near-duplicate stubs.
        let existing_nodes = graph
            .search_nodes(Some(crate::memory::graph::NodeType::Concept), Some(&title))
            .unwrap_or_default();
        let reused = existing_nodes
            .into_iter()
            .find(|n| n.title.to_lowercase() == title.to_lowercase());

        let node = if let Some(existing) = reused {
            // Bump confidence metadata if we have a higher value
            if let Some(mut meta_obj) = existing
                .metadata
                .clone()
                .and_then(|v| v.as_object().cloned())
            {
                let prior_conf = meta_obj
                    .get("confidence")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                if prior_conf < 0.75 {
                    meta_obj.insert("confidence".to_string(), serde_json::json!(0.75));
                    let _ = graph.update_node(
                        &existing.id,
                        None,
                        Some(serde_json::Value::Object(meta_obj)),
                    );
                }
            }
            info!(
                "Distiller: reusing existing concept node '{}' ({})",
                title,
                &existing.id[..8]
            );
            existing
        } else {
            match graph.create_node(crate::memory::graph::NodeType::Concept, &title, Some(meta)) {
                Ok(n) => n,
                Err(e) => {
                    warn!("Distiller: node create failed — {}", e);
                    continue;
                }
            }
        };

        let embed_text = format!("{} {}", title, content);
        pending.push((node, content, category, embed_text));
    }

    if pending.is_empty() {
        return;
    }

    // Phase 2: batch-embed all pending facts in one call. get_embeddings_batch
    // still iterates under the hood, but providers that support batch input
    // can be plugged in later without touching this caller.
    let texts: Vec<&str> = pending.iter().map(|(_, _, _, t)| t.as_str()).collect();
    let vectors = llm.get_embeddings_batch(texts).await.unwrap_or_default();

    // Phase 3: persist nodes + object memory, attach embeddings where available.
    for (idx, (node, content, category, _embed)) in pending.into_iter().enumerate() {
        if let Some(vec) = vectors.get(idx) {
            if !vec.is_empty() {
                let _ = emb_store.store(
                    &node.id,
                    crate::memory::embedding::EmbeddingType::Summary,
                    vec,
                    Some(&content),
                );
            }
        }

        let obj = crate::memory::object::ObjectMemory {
            id: Uuid::new_v4().to_string(),
            node_id: node.id.clone(),
            summary: Some(content.clone()),
            key_facts: vec![content],
            extracted_structure: None,
            code_signatures: None,
            todos: vec![],
            ui_snapshot: None,
            tags: vec![category, "distiller".to_string()],
            content_hash: None,
            last_indexed: 0,
            access_count: 0,
        };
        let _ = obj_store.upsert(&node.id, &obj);
    }
}

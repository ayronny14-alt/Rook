use anyhow::Result;
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::Mutex;
use tracing::warn;
use uuid::Uuid;

use super::HandlerCtx;
use crate::ipc::protocol::{IPCResponse, PendingEditDiff, TokenUsage, ToolCallResult};
use crate::llm::client::LLMClient;
use crate::memory::graph::GraphMemory;
use crate::memory::object::ObjectMemoryStore;

/// Rough characters-per-token estimate.
const CHARS_PER_TOKEN: usize = 4;
/// Leave at least this many tokens free for the response.
const HISTORY_MAX_TOKENS: usize = 90_000;

/// Best-effort provider tag from a base URL for the error log.
fn detect_provider(base_url: &str) -> &'static str {
    let u = base_url.to_ascii_lowercase();
    if u.contains("groq.com") {
        "groq"
    } else if u.contains("openai.com") {
        "openai"
    } else if u.contains("anthropic.com") {
        "anthropic"
    } else if u.contains("mistral.ai") {
        "mistral"
    } else if u.contains("together.") {
        "together"
    } else if u.contains("deepseek.com") {
        "deepseek"
    } else if u.contains("localhost") || u.contains("127.0.0.1") {
        "local"
    } else {
        "unknown"
    }
}

/// Per-conversation session todo lists. The AI writes to this via the
/// `todo_write` tool and we replay it into the system prompt at the start
/// of every team-mode iteration so the model stays on task across turns.
/// Status: "pending" | "in_progress" | "completed"
#[derive(Clone, serde::Serialize, serde::Deserialize, Debug)]
struct TodoItem {
    content: String,
    #[serde(rename = "activeForm", default)]
    active_form: String,
    status: String,
}

static SESSION_TODOS: std::sync::OnceLock<Mutex<HashMap<String, Vec<TodoItem>>>> =
    std::sync::OnceLock::new();

fn session_todos() -> &'static Mutex<HashMap<String, Vec<TodoItem>>> {
    SESSION_TODOS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Per-conversation working directory. The AI's `change_dir` / `get_cwd` tools
/// write and read this rather than mutating the process cwd (which leaks into
/// unrelated conversations and bg tasks).
static SESSION_CWD: std::sync::OnceLock<Mutex<HashMap<String, std::path::PathBuf>>> =
    std::sync::OnceLock::new();

fn session_cwd() -> &'static Mutex<HashMap<String, std::path::PathBuf>> {
    SESSION_CWD.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn get_session_cwd(conv_id: &str) -> std::path::PathBuf {
    let map = session_cwd().lock().ok();
    let default = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    map.as_ref()
        .and_then(|m| m.get(conv_id).cloned())
        .unwrap_or(default)
}

pub fn set_session_cwd(conv_id: &str, path: std::path::PathBuf) {
    if let Ok(mut map) = session_cwd().lock() {
        map.insert(conv_id.to_string(), path);
    }
}

/// Per-conversation file edit history with multi-level undo.
/// Keyed by (conv_id, absolute_path), holds a stack of snapshots (oldest first).
static FILE_SNAPSHOTS: std::sync::OnceLock<Mutex<HashMap<(String, String), Vec<String>>>> =
    std::sync::OnceLock::new();

fn file_snapshots() -> &'static Mutex<HashMap<(String, String), Vec<String>>> {
    FILE_SNAPSHOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Clean up all in-memory per-conversation state when a conversation is deleted.
pub fn clear_conversation_state(conversation_id: &str) {
    if let Ok(mut map) = session_todos().lock() {
        map.remove(conversation_id);
    }
    if let Ok(mut map) = session_cwd().lock() {
        map.remove(conversation_id);
    }
    if let Ok(mut map) = file_snapshots().lock() {
        map.retain(|(cid, _), _| cid != conversation_id);
    }
}

/// Push a snapshot of the current file content onto the undo stack.
/// Called BEFORE each edit so file_undo can step back through changes.
pub fn snapshot_file(conv_id: &str, path: &str) {
    let abs = std::fs::canonicalize(path)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.to_string());
    if let Ok(content) = std::fs::read_to_string(&abs) {
        if let Ok(mut map) = file_snapshots().lock() {
            let stack = map.entry((conv_id.to_string(), abs)).or_default();
            // Don't push if content is identical to the top of the stack
            if stack.last().map(|s| s.as_str()) != Some(&content) {
                // Cap at 20 levels to bound memory usage
                if stack.len() >= 20 {
                    stack.remove(0);
                }
                stack.push(content);
            }
        }
    }
}

/// Peek at the most recent snapshot (for file_diff comparison).
pub fn get_snapshot(conv_id: &str, path: &str) -> Option<String> {
    let abs = std::fs::canonicalize(path)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.to_string());
    file_snapshots()
        .lock()
        .ok()
        .and_then(|m| m.get(&(conv_id.to_string(), abs))?.first().cloned())
}

/// Pop the most recent snapshot (for file_undo - restores and removes from stack).
pub fn pop_snapshot(conv_id: &str, path: &str) -> Option<String> {
    let abs = std::fs::canonicalize(path)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.to_string());
    file_snapshots().lock().ok().and_then(|mut m| {
        let stack = m.get_mut(&(conv_id.to_string(), abs))?;
        stack.pop()
    })
}

/// Get remaining undo levels for a file.
pub fn undo_depth(conv_id: &str, path: &str) -> usize {
    let abs = std::fs::canonicalize(path)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.to_string());
    file_snapshots()
        .lock()
        .ok()
        .and_then(|m| m.get(&(conv_id.to_string(), abs)).map(|s| s.len()))
        .unwrap_or(0)
}

/// Get a public snapshot of the todos for a conversation - used by the UI to
/// render a live task list in the memory panel.
pub fn get_session_todos_json(conv_id: &str) -> Vec<serde_json::Value> {
    let map = session_todos().lock().ok();
    let todos = map
        .as_ref()
        .and_then(|m| m.get(conv_id))
        .cloned()
        .unwrap_or_default();
    todos
        .into_iter()
        .map(|t| {
            serde_json::json!({
                "content": t.content,
                "activeForm": t.active_form,
                "status": t.status,
            })
        })
        .collect()
}

fn format_todos_for_prompt(conv_id: &str) -> String {
    let map = session_todos().lock().ok();
    let todos = map
        .as_ref()
        .and_then(|m| m.get(conv_id))
        .cloned()
        .unwrap_or_default();
    if todos.is_empty() {
        return String::new();
    }
    let mut s = String::from("\n\n## Current task list (your TODOs)\n");
    for (i, t) in todos.iter().enumerate() {
        let marker = match t.status.as_str() {
            "completed" => "[x]",
            "in_progress" => "[~]",
            _ => "[ ]",
        };
        let label = if t.status == "in_progress" && !t.active_form.is_empty() {
            t.active_form.as_str()
        } else {
            t.content.as_str()
        };
        s.push_str(&format!("{}. {} {}\n", i + 1, marker, label));
    }
    s.push_str("\nKeep exactly one task `in_progress`. Mark each task `completed` immediately after finishing it. Use `todo_write` to update the list.\n");
    s
}

pub async fn handle_chat(
    ctx: &HandlerCtx,
    id: &str,
    conversation_id: Option<&str>,
    message: &str,
    agent_mode: Option<&str>,
    model: Option<&str>,
    system_prompt: Option<&str>,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
) -> Result<IPCResponse> {
    let conv_id = conversation_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    // Load prior conversation turns with a rolling-window strategy:
    //   • last 12 turns verbatim (immediate context)
    //   • older turns collapsed into a single extractive digest system message
    //     (pointers only - the model can call `search_memory` for detail)
    // Bounded at ~2k tokens regardless of conversation length.
    const RECENT_TURNS: usize = 12;
    const DIGEST_CHAR_LIMIT: usize = 1800;
    let conv_history: Vec<crate::llm::types::Message> = match ctx.memory.get_connection() {
        Ok(conn) => {
            let raw_pairs: Vec<(String, String)> = conn
                .prepare(
                    "SELECT role, content FROM messages \
                     WHERE conversation_id = ?1 ORDER BY created_at ASC LIMIT 200",
                )
                .and_then(|mut stmt| {
                    stmt.query_map(rusqlite::params![&conv_id], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })
                    .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
                })
                .unwrap_or_default();

            // Split into [older …, recent (last RECENT_TURNS)]
            let (older, recent) = if raw_pairs.len() > RECENT_TURNS {
                let split = raw_pairs.len() - RECENT_TURNS;
                let mut v = raw_pairs;
                let rest = v.split_off(split);
                (v, rest)
            } else {
                (Vec::new(), raw_pairs)
            };

            // Extractive digest of older turns: role + first 120 chars of each.
            // No LLM call, deterministic, bounded by DIGEST_CHAR_LIMIT.
            let mut kept: Vec<(String, String)> = Vec::new();
            if !older.is_empty() {
                let mut digest = String::from(
                    "Earlier in this conversation (digest - call search_memory for detail):\n",
                );
                for (role, content) in older.iter() {
                    let snippet: String = content
                        .chars()
                        .take(120)
                        .collect::<String>()
                        .replace('\n', " ");
                    digest.push_str(&format!("- {}: {}\n", role, snippet));
                    if digest.len() > DIGEST_CHAR_LIMIT {
                        digest.truncate(DIGEST_CHAR_LIMIT);
                        digest.push_str("…\n");
                        break;
                    }
                }
                kept.push(("system".to_string(), digest));
            }
            kept.extend(recent);
            if kept.is_empty() {
                // Absolute fallback: always include the single most recent exchange.
                kept = conn
                    .prepare(
                        "SELECT role, content FROM messages \
                         WHERE conversation_id = ?1 ORDER BY created_at DESC LIMIT 2",
                    )
                    .and_then(|mut stmt| {
                        stmt.query_map(rusqlite::params![&conv_id], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        })
                        .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
                    })
                    .unwrap_or_default()
                    .into_iter()
                    .rev()
                    .collect();
            }
            kept.into_iter()
                .map(|(role, content)| crate::llm::types::Message::text(&role, content))
                .collect()
        }
        Err(_) => vec![],
    };

    // Persist current user message
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if let Ok(conn) = ctx.memory.get_connection() {
            let title: String = message.chars().take(80).collect();
            let _ = conn.execute(
                "INSERT OR IGNORE INTO conversations (id, title, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
                rusqlite::params![&conv_id, &title, now],
            );
            let msg_id = Uuid::new_v4().to_string();
            let _ = conn.execute(
                "INSERT INTO messages (id, conversation_id, role, content, created_at) VALUES (?1, ?2, 'user', ?3, ?4)",
                rusqlite::params![&msg_id, &conv_id, message, now],
            );
            // bump updated_at so the sidebar orders recently-active chats on top
            let _ = conn.execute(
                "UPDATE conversations SET updated_at = ?1 WHERE id = ?2",
                rusqlite::params![now, &conv_id],
            );
        }
    }

    // Lean RAG: no bulk packet injection. The model gets a thin INDEX (top few
    // node titles) as a hint so it knows what to ask about, then calls
    // `search_memory` / `recall_detail` when it actually needs content.
    // This replaces the old 6000-token packet stuffing.
    let msg_trimmed = message.trim();
    let char_count = msg_trimmed.len();
    let word_count = msg_trimmed.split_whitespace().count();
    let is_trivial = word_count <= 3 && char_count <= 20;

    let (curated_context, curated_struct) = if is_trivial {
        tracing::debug!("trivial message - skipping RAG index hint");
        (None, None)
    } else {
        // Cheap top-K title query - no content, no summaries, no LLM summarization.
        // Budget: ~150 tokens total for the hint block.
        let curator = if ctx.gnn_available {
            crate::memory::context::ContextCurator::new(ctx.memory.clone())
        } else {
            crate::memory::context::ContextCurator::without_gnn(ctx.memory.clone())
        };
        match curator
            .curate_for_query(&ctx.llm, message, Some(24), Some(5))
            .await
        {
            Ok(items) if !items.is_empty() => {
                // Titles-only index hint. Keep under ~150 tokens.
                let mut hint = String::from(
                    "Memory index (titles only - call `search_memory` with a specific query for content):\n",
                );
                for (i, item) in items.iter().take(5).enumerate() {
                    let title = item.title.chars().take(80).collect::<String>();
                    hint.push_str(&format!("{}. [{}] {}\n", i + 1, item.node_type, title));
                }
                let nodes = serde_json::to_value(&items).unwrap_or(serde_json::Value::Null);
                let struct_json = serde_json::json!({ "memory": nodes });
                (Some(hint), Some(struct_json))
            }
            Ok(_) => (None, None),
            Err(err) => {
                warn!("RAG index query failed: {}", err);
                (None, None)
            }
        }
    };

    // Always emit ContextCurated before the first token so the UI panel
    // updates even when there are no memory nodes yet (empty state).
    {
        let nodes = curated_struct
            .as_ref()
            .and_then(|p| p.get("memory").cloned())
            .unwrap_or(serde_json::Value::Array(vec![]));
        let _ = ctx
            .chunk_tx
            .send(IPCResponse::ContextCurated {
                id: id.to_string(),
                conversation_id: conv_id.clone(),
                nodes,
            })
            .await;
    }

    // Fetch user_facts once, reuse for both UI emission and system-message injection.
    let user_facts: Vec<(String, String)> = ctx
        .memory
        .get_connection()
        .ok()
        .and_then(|conn| {
            conn.prepare("SELECT key, value FROM user_facts ORDER BY key")
                .and_then(|mut stmt| {
                    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                        .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
                })
                .ok()
        })
        .unwrap_or_default();

    {
        let facts_json: Vec<serde_json::Value> = user_facts
            .iter()
            .map(|(k, v)| serde_json::json!({ "key": k, "value": v }))
            .collect();
        let _ = ctx
            .chunk_tx
            .send(IPCResponse::UserFactsLoaded {
                id: id.to_string(),
                conversation_id: conv_id.clone(),
                facts: facts_json,
            })
            .await;
    }

    let (mut messages, warnings) =
        crate::ipc::guards::build_chat_messages_with_context(message, curated_context.as_deref())?;

    // Splice history between system and current user message
    if !conv_history.is_empty() {
        let current = messages.remove(messages.len() - 1);
        messages.extend(conv_history);
        messages.push(current);
    }

    // Truncate history if context window would overflow
    {
        let total_chars: usize = messages.iter().map(|m| m.content.len()).sum();
        if total_chars / CHARS_PER_TOKEN > HISTORY_MAX_TOKENS {
            while messages.len() > 2 {
                let chars: usize = messages.iter().map(|m| m.content.len()).sum();
                if chars / CHARS_PER_TOKEN <= HISTORY_MAX_TOKENS {
                    break;
                }
                // Skip all leading system messages to preserve injected context/instructions.
                // If nothing but system remains, stop - better an over-budget prompt than a
                // truncated instruction header.
                let Some(remove_idx) = messages.iter().position(|m| m.role.as_str() != "system")
                else {
                    break;
                };
                messages.remove(remove_idx);
            }
        }
    }

    // Prepend project instructions
    if let Some(instructions) = &ctx.project_instructions {
        messages.insert(
            0,
            crate::llm::types::Message::text(
                "system",
                format!("# Project Instructions\n\n{}", instructions),
            ),
        );
    }

    // Prepend user system prompt (from Settings → Chat)
    if let Some(sp) = system_prompt {
        if !sp.trim().is_empty() {
            messages.insert(0, crate::llm::types::Message::text("system", sp));
        }
    }

    // Inject active plugin context
    {
        let active_plugins = ctx
            .plugin_registry
            .list_active_mcps()
            .unwrap_or_default()
            .into_iter()
            .chain(
                ctx.plugin_registry
                    .list(None)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|p| {
                        p.enabled
                            && matches!(p.status, crate::plugins::registry::PluginStatus::Installed)
                    }),
            )
            .collect::<Vec<_>>();

        if !active_plugins.is_empty() {
            let summary: String = active_plugins
                .iter()
                .map(|p| {
                    format!(
                        "- {} [{}]: {}",
                        p.name,
                        p.plugin_type.as_str(),
                        p.description.as_deref().unwrap_or("(no description)")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            messages.push(crate::llm::types::Message::text(
                "system",
                format!(
                    "# Active Plugins\nThe following plugins are installed and enabled:\n{}\n\
                 You may reference these plugins when answering questions or suggesting tools.",
                    summary
                ),
            ));
        }

        let lower_msg = message.to_lowercase();
        if lower_msg.contains("plugin")
            || lower_msg.contains("mcp")
            || lower_msg.contains("skill")
            || lower_msg.contains("connector")
            || lower_msg.contains("install")
            || lower_msg.contains("extension")
        {
            if let Ok(search_results) = ctx.plugin_registry.search_active(message) {
                if !search_results.is_empty() {
                    let sr_summary: String = search_results
                        .iter()
                        .map(|p| {
                            format!(
                                "- {} ({}): {}",
                                p.name,
                                p.plugin_type.as_str(),
                                p.description.as_deref().unwrap_or("")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    messages.push(crate::llm::types::Message::text(
                        "system",
                        format!(
                            "# Relevant installed plugins for this query:\n{}",
                            sr_summary
                        ),
                    ));
                }
            }
        }
    }

    // Touch object memory + create co-occurrence edges for retrieved nodes
    let retrieved_node_ids: Vec<String> = curated_struct
        .as_ref()
        .and_then(|v| v.get("memory"))
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| item.get("node_id")?.as_str().map(String::from))
                .take(4)
                .collect()
        })
        .unwrap_or_default();
    if !retrieved_node_ids.is_empty() {
        let mem_bg = ctx.memory.clone();
        let nids = retrieved_node_ids.clone();
        tokio::spawn(async move {
            let obj_store = ObjectMemoryStore::new(mem_bg.clone());
            for nid in &nids {
                let _ = obj_store.touch_accessed(nid);
            }
            if nids.len() >= 2 {
                let graph = GraphMemory::new(mem_bg);
                for i in 0..nids.len() {
                    for j in (i + 1)..nids.len() {
                        let _ = graph.create_edge_if_not_exists(
                            &nids[i],
                            &nids[j],
                            crate::memory::graph::Relationship::RelatesTo,
                            0.3,
                        );
                    }
                }
            }
        });
    }

    // Auto-start Ollama for local models
    let local_prefixes = [
        "gemma",
        "llama",
        "mistral",
        "phi",
        "qwen",
        "deepseek",
        "codellama",
        "solar",
    ];
    let is_local_model = model
        .map(|m| {
            m.contains(':')
                || local_prefixes
                    .iter()
                    .any(|p| m.to_ascii_lowercase().starts_with(p))
        })
        .unwrap_or(false);
    if is_local_model {
        match ctx.gemma.try_start().await {
            Ok(false) => {
                return Ok(IPCResponse::Error {
                    id: id.to_string(),
                    message:
                        "Not enough free RAM to start Ollama. Free at least 6 GiB and try again."
                            .to_string(),
                })
            }
            Err(e) => {
                return Ok(IPCResponse::Error {
                    id: id.to_string(),
                    message: format!("Failed to start Ollama: {}", e),
                })
            }
            Ok(true) => {}
        }
    }

    // Resolve effective LLM: runtime override → per-request param overrides → env defaults.
    //
    // IMPORTANT: many providers (Groq, Anthropic) count max_tokens against their
    // per-minute rate limit BEFORE generation. A default of 8192 eats ~8k of the
    // budget regardless of whether the model ends up using it. Cap per mode:
    //   Chat/Assistant  → 2048  (most replies are 50-500 tokens)
    //   Code/Coder      → 4096  (longer code blocks)
    //   Team            → 8192  (agent loops can need it)
    let default_mode = agent_mode.unwrap_or("Chat");
    let mode_max_tokens: u32 = match default_mode {
        "Team" => 8192,
        "Code" | "Coder" => 4096,
        _ => 2048,
    };

    let effective_llm_storage;
    let effective_llm: &LLMClient = {
        let mut cfg = ctx
            .config_override
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(|| ctx.llm.base_config().clone());
        // Honor user-supplied max_tokens when reasonable, else cap to mode default.
        cfg.max_tokens = max_tokens.unwrap_or(mode_max_tokens).min(mode_max_tokens);
        if let Some(t) = temperature {
            cfg.temperature = t;
        }
        effective_llm_storage = LLMClient::new(cfg);
        &effective_llm_storage
    };

    // Lean user-profile hint: only the identity keys the model needs upfront
    // (name, pronouns, timezone). Everything else is retrievable via
    // `search_memory` when relevant. Full dump was ~1k tokens per turn.
    if !user_facts.is_empty() {
        let core_keys = ["name", "full_name", "pronouns", "timezone", "location"];
        let essentials: Vec<String> = user_facts
            .iter()
            .filter(|(k, _)| core_keys.contains(&k.to_ascii_lowercase().as_str()))
            .map(|(k, v)| format!("• {}: {}", k, v))
            .collect();

        if !essentials.is_empty() {
            let more = user_facts.len().saturating_sub(essentials.len());
            let mut hint = String::from("# User (essentials)\n");
            hint.push_str(&essentials.join("\n"));
            if more > 0 {
                hint.push_str(&format!(
                    "\n[+{} more facts available via search_memory]",
                    more
                ));
            }
            messages.push(crate::llm::types::Message::text("system", hint));
        } else {
            messages.push(crate::llm::types::Message::text(
                "system",
                format!(
                    "# User\n[{} facts stored - query with search_memory if needed]",
                    user_facts.len()
                ),
            ));
        }
    }

    // Small-to-large router: a trivial message ("hi", "thanks", "ok") doesn't
    // need the 25-iteration Team apparatus. Downgrade to Chat mode so we run
    // one cheap call instead of dragging 35 tool schemas through a loop.
    let requested_mode = agent_mode.unwrap_or("Chat");
    let effective_mode = if requested_mode == "Team" && is_trivial_query(message) {
        "Chat"
    } else {
        requested_mode
    };

    match effective_mode {
        "Chat" | "Assistant" => {
            handle_chat_mode(
                ctx,
                id,
                &conv_id,
                message,
                model,
                messages,
                warnings,
                curated_struct,
                effective_llm,
            )
            .await
        }
        "Code" | "Coder" => {
            handle_coder_mode(
                ctx,
                id,
                &conv_id,
                model,
                messages,
                warnings,
                curated_struct,
                effective_llm,
            )
            .await
        }
        "Team" => {
            handle_team_mode(
                ctx,
                id,
                &conv_id,
                model,
                messages,
                warnings,
                curated_struct,
                effective_llm,
            )
            .await
        }
        other => {
            handle_streaming_fallback(
                ctx,
                id,
                &conv_id,
                model,
                messages,
                warnings,
                curated_struct,
                other,
                effective_llm,
            )
            .await
        }
    }
}

async fn handle_chat_mode(
    ctx: &HandlerCtx,
    id: &str,
    conv_id: &str,
    message: &str,
    model: Option<&str>,
    mut messages: Vec<crate::llm::types::Message>,
    warnings: Vec<String>,
    _curated_struct: Option<serde_json::Value>,
    effective_llm: &LLMClient,
) -> Result<IPCResponse> {
    let system_text = r#"You are Rook - a helpful assistant with long-term memory.

You have two memory tools you can call alongside your reply:
• store_memory - save important context, decisions, or project details for future retrieval
• store_user_fact - save/update a fact about the user (name, preferences, tools, etc.) in their global profile

Guidelines:
• When the user shares personal info (name, preferences, etc.), store it with store_user_fact.
• When important decisions, project context, or useful knowledge comes up, store it with store_memory.
• You may call memory tools AND provide a text reply in the same response.
• Keep replies concise and rely on memory context when available.
• Do NOT mention that you are storing things unless the user asks."#;
    messages.insert(0, crate::llm::types::Message::text("system", system_text));

    // Memory tools are handled by the background distiller, so we stream
    // without tool definitions for true token-by-token output.
    let stream_result = effective_llm
        .chat_stream_for_model(messages.clone(), model)
        .await;

    if let Err(ref e) = stream_result {
        let est = messages.iter().map(|m| m.content.len()).sum::<usize>() / 4;
        crate::error_log::record(crate::error_log::ErrorContext {
            source: "llm_api::chat_mode_stream",
            message: &format!("{}", e),
            conversation_id: Some(conv_id),
            model,
            provider: Some(detect_provider(&effective_llm.base_config().base_url)),
            agent_mode: Some("Chat"),
            max_tokens: Some(effective_llm.base_config().max_tokens),
            prompt_preview: Some(crate::error_log::preview_messages(&messages)),
            est_input_tokens: Some(est),
            extra: None,
        });
    }

    let mut content = String::new();
    let mut usage: Option<TokenUsage> = None;

    match stream_result {
        Ok(Some(resp)) => {
            let mut byte_stream = resp.bytes_stream();
            let mut buffer = String::new();
            let mut reasoning_buf = String::new();
            let mut thinking_emitted = false;
            let mut cancelled = false;

            loop {
                tokio::select! {
                    biased;
                    _ = ctx.cancel_token.cancelled() => {
                        cancelled = true;
                        break;
                    }
                    chunk_opt = byte_stream.next() => {
                        let chunk_result = match chunk_opt {
                            Some(c) => c,
                            None => break,
                        };
                        let bytes = match chunk_result {
                            Ok(b) => b,
                            Err(e) => { warn!("Chat SSE read error: {}", e); break; }
                        };
                        buffer.push_str(&String::from_utf8_lossy(&bytes));

                        while let Some(pos) = buffer.find('\n') {
                            let line = buffer[..pos].trim_end_matches('\r').to_string();
                            buffer = buffer[pos + 1..].to_string();

                            if line.is_empty() || line.starts_with(':') { continue; }
                            let data = if let Some(rest) = line.strip_prefix("data: ") { rest } else if let Some(rest) = line.strip_prefix("data:") { rest }
                                       else { continue; };
                            if data.trim() == "[DONE]" || data.trim().is_empty() { continue; }

                            if let Ok(chunk) = serde_json::from_str::<crate::llm::types::StreamChunkResponse>(data) {
                                // Capture usage from the final chunk if present
                                if let Some(u) = &chunk.usage {
                                    usage = Some(TokenUsage {
                                        prompt_tokens: u.prompt_tokens,
                                        completion_tokens: u.completion_tokens,
                                        total_tokens: u.total_tokens,
                                    });
                                }

                                let delta = chunk.choices.first().and_then(|c| c.delta.as_ref());

                                // Collect reasoning/thinking deltas
                                if let Some(thought_delta) = delta.and_then(|d| d.thinking_delta()) {
                                    if !thought_delta.is_empty() {
                                        reasoning_buf.push_str(thought_delta);
                                    }
                                }

                                if let Some(token) = delta.and_then(|d| d.content.as_ref()) {
                                    if !token.is_empty() {
                                        // Flush thinking before first content token
                                        if !thinking_emitted && !reasoning_buf.is_empty() {
                                            let _ = ctx.chunk_tx.send(IPCResponse::ChatThinking {
                                                id: id.to_string(),
                                                thinking: reasoning_buf.clone(),
                                            }).await;
                                            thinking_emitted = true;
                                        }
                                        content.push_str(token);
                                        let _ = ctx.chunk_tx.send(IPCResponse::ChatChunk {
                                            id: id.to_string(),
                                            token: token.clone(),
                                        }).await;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Emit buffered thinking if model only produced reasoning
            if !thinking_emitted && !reasoning_buf.is_empty() {
                let _ = ctx
                    .chunk_tx
                    .send(IPCResponse::ChatThinking {
                        id: id.to_string(),
                        thinking: reasoning_buf,
                    })
                    .await;
            }

            drop(byte_stream);

            if cancelled {
                let _ = ctx
                    .chunk_tx
                    .send(IPCResponse::Cancelled {
                        id: id.to_string(),
                        target_id: id.to_string(),
                    })
                    .await;
                return Ok(IPCResponse::ChatDone {
                    id: id.to_string(),
                    conversation_id: conv_id.to_string(),
                    usage: None,
                });
            }

            // Fallback: if SSE produced no content but left a non-SSE JSON body
            if content.is_empty() && !buffer.trim().is_empty() {
                if let Ok(fallback) =
                    serde_json::from_str::<crate::llm::types::ChatCompletionResponse>(&buffer)
                {
                    content = fallback
                        .choices
                        .first()
                        .and_then(|c| c.message.text_content())
                        .unwrap_or_default();
                    if let Some(u) = &fallback.usage {
                        usage = Some(TokenUsage {
                            prompt_tokens: u.prompt_tokens,
                            completion_tokens: u.completion_tokens,
                            total_tokens: u.total_tokens,
                        });
                    }
                    // Replay as word-split chunks
                    if !content.is_empty() {
                        for word in content.split_inclusive(char::is_whitespace) {
                            let _ = ctx
                                .chunk_tx
                                .send(IPCResponse::ChatChunk {
                                    id: id.to_string(),
                                    token: word.to_string(),
                                })
                                .await;
                        }
                    }
                }
            }
        }
        Ok(None) => {
            // Mock mode - no streaming, do non-streaming fallback
            let response = effective_llm.chat(messages).await?;
            content = response
                .choices
                .first()
                .and_then(|c| c.message.text_content())
                .unwrap_or_default();
            if let Some(u) = &response.usage {
                usage = Some(TokenUsage {
                    prompt_tokens: u.prompt_tokens,
                    completion_tokens: u.completion_tokens,
                    total_tokens: u.total_tokens,
                });
            }
            if !content.is_empty() {
                for word in content.split_inclusive(char::is_whitespace) {
                    let _ = ctx
                        .chunk_tx
                        .send(IPCResponse::ChatChunk {
                            id: id.to_string(),
                            token: word.to_string(),
                        })
                        .await;
                }
            }
        }
        Err(e) => {
            warn!("Chat stream failed, falling back to non-streaming: {}", e);
            let response = effective_llm.chat(messages).await?;
            content = response
                .choices
                .first()
                .and_then(|c| c.message.text_content())
                .unwrap_or_default();
            if let Some(u) = &response.usage {
                usage = Some(TokenUsage {
                    prompt_tokens: u.prompt_tokens,
                    completion_tokens: u.completion_tokens,
                    total_tokens: u.total_tokens,
                });
            }
            if !content.is_empty() {
                for word in content.split_inclusive(char::is_whitespace) {
                    let _ = ctx
                        .chunk_tx
                        .send(IPCResponse::ChatChunk {
                            id: id.to_string(),
                            token: word.to_string(),
                        })
                        .await;
                }
            }
        }
    }

    if !warnings.is_empty() && !content.is_empty() {
        content = format!("[context guard] {}\n\n{}", warnings.join(" "), content);
    }

    persist_assistant_message(&ctx.memory, conv_id, &content);

    if !content.is_empty() {
        let mem = ctx.memory.clone();
        let um = message.to_string();
        let ac = content.clone();
        tokio::task::spawn_blocking(move || {
            crate::memory::extractor::run(&mem, &um, &ac);
        });
    }

    // Auto-title: fire-and-forget; only fires on 2nd user message.
    // IMPORTANT: we must NOT hold a chunk_tx clone in this task after it's done -
    // if the LLM call hangs, the forwarder task stays open and blocks the IPC server.
    // We drop tx immediately after sending (or on timeout) using a 12-second limit.
    {
        let mem = ctx.memory.clone();
        let llm = ctx.llm.clone();
        let tx = ctx.chunk_tx.clone();
        let conv_id_owned = conv_id.to_string();
        let id_owned = id.to_string();
        tokio::spawn(async move {
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(12),
                crate::ipc::handlers::conversations::auto_title_if_ready(
                    &mem,
                    &llm,
                    &conv_id_owned,
                ),
            )
            .await;
            if let Ok(Some(title)) = result {
                let _ = tx
                    .send(IPCResponse::ConversationTitled {
                        id: id_owned,
                        conversation_id: conv_id_owned,
                        title,
                    })
                    .await;
            }
            drop(tx); // ensure tx is released promptly so forwarder can finish
        });
    }

    Ok(IPCResponse::ChatDone {
        id: id.to_string(),
        conversation_id: conv_id.to_string(),
        usage,
    })
}

async fn handle_coder_mode(
    ctx: &HandlerCtx,
    id: &str,
    conv_id: &str,
    model: Option<&str>,
    mut messages: Vec<crate::llm::types::Message>,
    warnings: Vec<String>,
    curated_struct: Option<serde_json::Value>,
    effective_llm: &LLMClient,
) -> Result<IPCResponse> {
    let system_text = "You are a coding agent. Prefer detailed code suggestions, include reasoning, and provide runnable snippets when appropriate. If you want to make a change to the codebase, return a tool call using the provided code_edit or terminal tool definitions.";
    // Capture user message now before messages is consumed by the LLM call.
    let user_message_for_distill = messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.clone())
        .unwrap_or_default();
    messages.insert(0, crate::llm::types::Message::text("system", system_text));

    let graph = GraphMemory::new(ctx.memory.clone());
    if let Ok(files) = graph.search_nodes(
        Some(crate::memory::graph::NodeType::File),
        messages.last().map(|m| m.content.as_str()),
    ) {
        if !files.is_empty() {
            let mut files_summary = String::from("\n\nRelevant code files:\n");
            for f in files.into_iter().take(6) {
                files_summary.push_str(&format!("- {}\n", f.title));
            }
            messages.push(crate::llm::types::Message::text("system", files_summary));
        }
    }

    let tools_defs = vec![
        crate::llm::types::ToolDefinition {
            tool_type: "function".to_string(),
            function: crate::llm::types::FunctionDefinition {
                name: "code_edit".to_string(),
                description: "Edit or modify a file in the workspace. Supported actions: append, insert_at_line, search_replace.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "action": {"type":"string"},
                        "path": {"type":"string"},
                        "line": {"type":"integer"},
                        "content": {"type":"string"},
                        "search": {"type":"string"},
                        "replace": {"type":"string"}
                    },
                    "required": ["action","path"]
                }),
            },
        },
        crate::llm::types::ToolDefinition {
            tool_type: "function".to_string(),
            function: crate::llm::types::FunctionDefinition {
                name: "terminal_execute".to_string(),
                description: "Execute a shell command in the project's workspace. Returns stdout/stderr.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {"command": {"type":"string"}},
                    "required": ["command"]
                }),
            },
        },
    ];

    let response = tokio::select! {
        biased;
        _ = ctx.cancel_token.cancelled() => {
            let _ = ctx.chunk_tx.send(IPCResponse::Cancelled {
                id: id.to_string(),
                target_id: id.to_string(),
            }).await;
            return Ok(IPCResponse::ChatDone {
                id: id.to_string(),
                conversation_id: conv_id.to_string(),
                usage: None,
            });
        }
        res = effective_llm.chat_with_tools_for_model(messages.clone(), tools_defs, model) => res?,
    };

    let usage = response.usage.as_ref().map(|u| TokenUsage {
        prompt_tokens: u.prompt_tokens,
        completion_tokens: u.completion_tokens,
        total_tokens: u.total_tokens,
    });

    let mut content = response
        .choices
        .first()
        .and_then(|c| c.message.text_content())
        .unwrap_or_default();

    if !warnings.is_empty() {
        content = format!("[context guard] {}\n\n{}", warnings.join(" "), content);
    }

    if let Some(calls) = response
        .choices
        .first()
        .and_then(|c| c.message.tool_calls.as_ref())
    {
        let mut diffs: Vec<PendingEditDiff> = Vec::new();
        for tc in calls.iter() {
            let name = tc.function.name.clone();
            let args_val: serde_json::Value =
                serde_json::from_str(&tc.function.arguments).unwrap_or(serde_json::Value::Null);
            let path = args_val
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let diff = if name == "code_edit" {
                let original = tokio::fs::read_to_string(&path).await.unwrap_or_default();
                let action = args_val
                    .get("action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let simulated = match action {
                    "search_replace" => {
                        let search = args_val
                            .get("search")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let replace = args_val
                            .get("replace")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        original.replacen(search, replace, 1)
                    }
                    "append" => {
                        let c = args_val
                            .get("content")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        format!("{}\n{}", original.trim_end_matches('\n'), c)
                    }
                    _ => original.clone(),
                };
                crate::tools::ToolExecutor::compute_diff(&original, &simulated, &path)
            } else {
                args_val
                    .get("command")
                    .and_then(|v| v.as_str())
                    .map(|cmd| format!("$ {}", cmd))
                    .unwrap_or_default()
            };
            diffs.push(PendingEditDiff {
                tool_name: name,
                path,
                diff,
                args: args_val,
            });
        }

        persist_assistant_message(&ctx.memory, conv_id, &content);
        if let Ok(mut pt) = ctx.pending_tools.lock() {
            pt.insert(conv_id.to_string(), diffs.clone());
        }
        return Ok(IPCResponse::PendingToolApproval {
            id: id.to_string(),
            conversation_id: conv_id.to_string(),
            diffs,
        });
    }

    persist_assistant_message(&ctx.memory, conv_id, &content);
    // code-based fact extraction. regex, not vibes.
    if !content.is_empty() {
        let mem = ctx.memory.clone();
        let um = user_message_for_distill.clone();
        let ac = content.clone();
        tokio::task::spawn_blocking(move || {
            crate::memory::extractor::run(&mem, &um, &ac);
        });
    }
    Ok(IPCResponse::Chat {
        id: id.to_string(),
        conversation_id: conv_id.to_string(),
        content,
        tool_calls: None,
        context_packet: curated_struct,
        usage,
    })
}

async fn handle_team_mode(
    ctx: &HandlerCtx,
    id: &str,
    conv_id: &str,
    model: Option<&str>,
    mut messages: Vec<crate::llm::types::Message>,
    warnings: Vec<String>,
    curated_struct: Option<serde_json::Value>,
    effective_llm: &LLMClient,
) -> Result<IPCResponse> {
    let _ = warnings; // currently informational only
                      // Capture user message before messages is mutated by system inserts.
    let user_message_for_distill = messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.clone())
        .unwrap_or_default();
    let system_text = r#"You are Rook - an autonomous software engineer agent.

FILES & NAV:  file_read, file_write, code_edit, file_diff, file_undo, list_files, change_dir, get_cwd
SEARCH:       glob, grep, search_in_file, outline_file
SHELLS:       shell_spawn, shell_exec, shell_read, shell_kill, shell_list, terminal_execute
GIT/WEB:      git_status, git_diff, git_log, git_branch, web_search, fetch_url
BROWSER:      browser_navigate, browser_click, browser_type, browser_evaluate, browser_screenshot
MEMORY:       store_memory, store_user_fact
TASKS:        todo_write (ONLY for complex 3+ step work)

ACT IMMEDIATELY. Use tools without narrating every step.

• Single-step tasks: just do them, no todo list.
• Complex multi-step (3+ distinct steps): one `todo_write` plan, then execute.
• Navigation order: `glob` → `grep` → `outline_file` → `file_read` for specific lines.
• Shells: the default shell persists cwd across calls. Need a second one for a dev server? `shell_spawn` name="server", then `shell_exec` shell="server" run_in_background=true.
• For long commands (>30s), use `run_in_background: true` and poll with `shell_read`.
• Before editing a file, use `file_read` first so `file_undo` and `file_diff` work.
• Call multiple independent tools in ONE response - they'll run in parallel.
• Finish with plain text (no trailing tool calls)."#;
    messages.insert(0, crate::llm::types::Message::text("system", system_text));

    // Inject the live todo list as a fresh system message at the top of each
    // iteration so the model never loses sight of what it's working on.
    let todos_text = format_todos_for_prompt(conv_id);
    if !todos_text.is_empty() {
        messages.insert(1, crate::llm::types::Message::text("system", &todos_text));
    }

    // Inject curated memory context - lets Team mode start with prior project
    // knowledge instead of starting cold every session.
    if let Some(ref packet) = curated_struct {
        if let Some(mem_nodes) = packet.get("memory").and_then(|v| v.as_array()) {
            if !mem_nodes.is_empty() {
                let mut context_text =
                    String::from("## Relevant prior context (from memory graph)\n\n");
                for (i, node) in mem_nodes.iter().take(8).enumerate() {
                    let title = node
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(untitled)");
                    let summary = node.get("summary").and_then(|v| v.as_str()).unwrap_or("");
                    let ntype = node.get("node_type").and_then(|v| v.as_str()).unwrap_or("");
                    context_text.push_str(&format!("{}. [{}] {}\n", i + 1, ntype, title));
                    if !summary.is_empty() {
                        let short: String = summary.chars().take(200).collect();
                        context_text.push_str(&format!("   {}\n", short));
                    }
                }
                context_text.push_str("\nUse this context but verify before relying on it.\n");
                messages.insert(1, crate::llm::types::Message::text("system", context_text));
            }
        }
    }

    // User facts were already injected by handle_chat before dispatching here - no re-fetch.

    // Tell the AI what its current working directory is at the start of every turn.
    let cwd_text = format!(
        "Current working directory: {}",
        get_session_cwd(conv_id).display()
    );
    messages.insert(1, crate::llm::types::Message::text("system", cwd_text));

    // Gemini / Vertex thought_signature continuity is handled by echoing the
    // `reasoning` field back on every assistant turn (Message::reasoning is
    // serialised to the provider). If the gateway strips that field, we fall
    // back to tool-less reasoning for vertex/bard routes as a safety net.
    let effective_model_name = model
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| effective_llm.base_config().model.to_ascii_lowercase());
    let tools_supported = !effective_model_name.contains("bard")
        && std::env::var("ROOK_DISABLE_GEMINI_TOOLS").ok().as_deref() != Some("1");

    let all_tools = if tools_supported {
        build_all_tools()
    } else {
        vec![]
    };

    if !tools_supported {
        let _ = ctx
            .chunk_tx
            .send(IPCResponse::ChatChunk {
                id: id.to_string(),
                token: format!(
                    "*note: tool calls disabled for `{}` - reasoning-only mode.*\n\n",
                    effective_model_name
                ),
            })
            .await;
    }

    const MAX_ITERATIONS: usize = 25;
    let mut iteration = 0;
    let mut final_content = String::new();
    let mut final_usage: Option<TokenUsage> = None;

    // Progressive tool-drop state: track tools the model has actually called so
    // we can shrink the registry on iteration 2+. Always-on tools (search_memory,
    // file_read) plus whatever the model used stay; everything else gets dropped.
    let mut tools_used: std::collections::HashSet<String> = std::collections::HashSet::new();
    const PINNED_TOOLS: &[&str] = &[
        "search_memory",
        "file_read",
        "grep",
        "glob",
        "list_files",
        "get_cwd",
    ];

    loop {
        iteration += 1;
        if iteration > MAX_ITERATIONS {
            let _ = ctx
                .chunk_tx
                .send(IPCResponse::ChatChunk {
                    id: id.to_string(),
                    token: "\n\n[Reached maximum tool iterations - stopping.]".to_string(),
                })
                .await;
            break;
        }

        // After iteration 1 the model has committed to an approach.
        // Narrow the tool set to pinned essentials + whatever it has used so far.
        // This cuts input tokens by 60-80% per iteration without sacrificing capability.
        let iteration_tools: Vec<crate::llm::types::ToolDefinition> = if iteration <= 1 {
            all_tools.clone()
        } else {
            all_tools
                .iter()
                .filter(|t| {
                    PINNED_TOOLS.contains(&t.function.name.as_str())
                        || tools_used.contains(&t.function.name)
                })
                .cloned()
                .collect()
        };

        let stream_result = effective_llm
            .chat_stream_with_tools_for_model(messages.clone(), iteration_tools, model)
            .await;

        let mut content = String::new();
        let mut reasoning_buf = String::new();
        let mut thinking_emitted = false;
        let mut content_streamed = false; // true if content was already sent as SSE ChatChunks
                                          // Tool call accumulators: keyed by index
        let mut tc_ids: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
        let mut tc_names: std::collections::HashMap<usize, String> =
            std::collections::HashMap::new();
        let mut tc_args: std::collections::HashMap<usize, String> =
            std::collections::HashMap::new();
        let mut cancelled = false;

        match stream_result {
            Ok(Some(resp)) => {
                use futures::StreamExt;
                let mut byte_stream = resp.bytes_stream();
                let mut buffer = String::new();

                loop {
                    tokio::select! {
                        biased;
                        _ = ctx.cancel_token.cancelled() => { cancelled = true; break; }
                        chunk_opt = byte_stream.next() => {
                            let bytes = match chunk_opt {
                                Some(Ok(b)) => b,
                                Some(Err(e)) => { warn!("Team SSE read error: {}", e); break; }
                                None => break,
                            };
                            buffer.push_str(&String::from_utf8_lossy(&bytes));

                            while let Some(pos) = buffer.find('\n') {
                                let line = buffer[..pos].trim_end_matches('\r').to_string();
                                buffer = buffer[pos + 1..].to_string();
                                if line.is_empty() || line.starts_with(':') { continue; }
                                let data = if let Some(rest) = line.strip_prefix("data: ") { rest } else if let Some(rest) = line.strip_prefix("data:") { rest }
                                           else { continue; };
                                if data.trim() == "[DONE]" || data.trim().is_empty() { continue; }

                                if let Ok(chunk) = serde_json::from_str::<crate::llm::types::StreamChunkResponse>(data) {
                                    if let Some(u) = &chunk.usage {
                                        final_usage = Some(TokenUsage {
                                            prompt_tokens: u.prompt_tokens,
                                            completion_tokens: u.completion_tokens,
                                            total_tokens: u.total_tokens,
                                        });
                                    }
                                    let delta = chunk.choices.first().and_then(|c| c.delta.as_ref());

                                    // Thinking deltas
                                    if let Some(thought) = delta.and_then(|d| d.thinking_delta()) {
                                        if !thought.is_empty() { reasoning_buf.push_str(thought); }
                                    }

                                    // Content deltas - stream to UI immediately
                                    if let Some(token) = delta.and_then(|d| d.content.as_ref()) {
                                        if !token.is_empty() {
                                            if !thinking_emitted && !reasoning_buf.is_empty() {
                                                let _ = ctx.chunk_tx.send(IPCResponse::ChatThinking {
                                                    id: id.to_string(), thinking: reasoning_buf.clone(),
                                                }).await;
                                                thinking_emitted = true;
                                            }
                                            content.push_str(token);
                                            let _ = ctx.chunk_tx.send(IPCResponse::ChatChunk {
                                                id: id.to_string(), token: token.clone(),
                                            }).await;
                                            content_streamed = true;
                                        }
                                    }

                                    // Tool call deltas - accumulate fragments
                                    if let Some(tcs) = delta.and_then(|d| d.tool_calls.as_ref()) {
                                        for tc_delta in tcs {
                                            let idx = tc_delta.index;
                                            if let Some(ref id_str) = tc_delta.id {
                                                tc_ids.insert(idx, id_str.clone());
                                            }
                                            if let Some(ref f) = tc_delta.function {
                                                if let Some(ref name) = f.name {
                                                    tc_names.insert(idx, name.clone());
                                                }
                                                if let Some(ref args_frag) = f.arguments {
                                                    tc_args.entry(idx).or_default().push_str(args_frag);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                drop(byte_stream);

                // Fallback: non-SSE JSON body
                if content.is_empty() && tc_names.is_empty() && !buffer.trim().is_empty() {
                    if let Ok(fallback) =
                        serde_json::from_str::<crate::llm::types::ChatCompletionResponse>(&buffer)
                    {
                        content = fallback
                            .choices
                            .first()
                            .and_then(|c| c.message.text_content())
                            .unwrap_or_default();
                        if let Some(u) = &fallback.usage {
                            final_usage = Some(TokenUsage {
                                prompt_tokens: u.prompt_tokens,
                                completion_tokens: u.completion_tokens,
                                total_tokens: u.total_tokens,
                            });
                        }
                        if let Some(calls) = fallback
                            .choices
                            .first()
                            .and_then(|c| c.message.tool_calls.as_ref())
                        {
                            for (i, tc) in calls.iter().enumerate() {
                                tc_ids.insert(i, tc.id.clone());
                                tc_names.insert(i, tc.function.name.clone());
                                tc_args.insert(i, tc.function.arguments.clone());
                            }
                        }
                    }
                }
            }
            Ok(None) => {
                // Mock mode fallback
                let response = effective_llm
                    .chat_with_tools_for_model(messages.clone(), all_tools.clone(), model)
                    .await?;
                content = response
                    .choices
                    .first()
                    .and_then(|c| c.message.text_content())
                    .unwrap_or_default();
                if let Some(u) = &response.usage {
                    final_usage = Some(TokenUsage {
                        prompt_tokens: u.prompt_tokens,
                        completion_tokens: u.completion_tokens,
                        total_tokens: u.total_tokens,
                    });
                }
                if let Some(calls) = response
                    .choices
                    .first()
                    .and_then(|c| c.message.tool_calls.as_ref())
                {
                    for (i, tc) in calls.iter().enumerate() {
                        tc_ids.insert(i, tc.id.clone());
                        tc_names.insert(i, tc.function.name.clone());
                        tc_args.insert(i, tc.function.arguments.clone());
                    }
                }
            }
            Err(e) => {
                let err_str = e.to_string();
                let hint = if err_str.contains("thought_signature") {
                    format!("⚠ This model ({}) requires Vertex AI thought signatures which aren't supported via this gateway. Switch to an OpenAI-compatible model (e.g. gpt-4o) in Settings to use Team mode tools.", effective_model_name)
                } else {
                    format!(
                        "⚠ Tool call failed: {}\n\nTry rephrasing or switching to Chat mode.",
                        err_str
                    )
                };
                let _ = ctx
                    .chunk_tx
                    .send(IPCResponse::ChatChunk {
                        id: id.to_string(),
                        token: format!("\n\n{}", hint),
                    })
                    .await;
                break;
            }
        }

        if !thinking_emitted && !reasoning_buf.is_empty() {
            let _ = ctx
                .chunk_tx
                .send(IPCResponse::ChatThinking {
                    id: id.to_string(),
                    thinking: reasoning_buf.clone(),
                })
                .await;
        }

        if cancelled {
            let _ = ctx
                .chunk_tx
                .send(IPCResponse::Cancelled {
                    id: id.to_string(),
                    target_id: id.to_string(),
                })
                .await;
            return Ok(IPCResponse::ChatDone {
                id: id.to_string(),
                conversation_id: conv_id.to_string(),
                usage: None,
            });
        }

        // Stream any non-streamed content (mock mode / fallback only - SSE already sent tokens)
        if !content.is_empty() && !content_streamed {
            for word in content.split_inclusive(char::is_whitespace) {
                let _ = ctx
                    .chunk_tx
                    .send(IPCResponse::ChatChunk {
                        id: id.to_string(),
                        token: word.to_string(),
                    })
                    .await;
            }
        }
        final_content.push_str(&content);

        // Reassemble tool calls from accumulated deltas
        let mut assembled_calls: Vec<crate::llm::types::ToolCallDefinition> = Vec::new();
        let max_idx = tc_names.keys().copied().max().unwrap_or(0);
        for idx in 0..=max_idx {
            if let Some(name) = tc_names.get(&idx) {
                assembled_calls.push(crate::llm::types::ToolCallDefinition {
                    id: tc_ids
                        .get(&idx)
                        .cloned()
                        .unwrap_or_else(|| format!("call_{}", idx)),
                    call_type: "function".to_string(),
                    function: crate::llm::types::FunctionCall {
                        name: name.clone(),
                        arguments: tc_args
                            .get(&idx)
                            .cloned()
                            .unwrap_or_else(|| "{}".to_string()),
                    },
                });
            }
        }

        // If no tool calls, we're done
        if assembled_calls.is_empty() {
            break;
        }
        // Remember which tools got used so progressive tool-drop on the next
        // iteration keeps them available.
        for c in &assembled_calls {
            tools_used.insert(c.function.name.clone());
        }
        let calls = &assembled_calls;

        messages.push(crate::llm::types::Message {
            role: "assistant".to_string(),
            content: content.clone(),
            content_blocks: None,
            tool_calls: Some(calls.clone()),
            // Echo the accumulated reasoning back. Gemini 3 / Vertex OpenAI-compat
            // requires thought_signature continuity between tool-call turns or it
            // 400s; other providers ignore the field. Cheap to send either way.
            reasoning: if reasoning_buf.is_empty() {
                None
            } else {
                Some(serde_json::Value::String(reasoning_buf.clone()))
            },
            ..Default::default()
        });

        use futures::future::join_all;

        // Tuple: (original_index, tool_call_id, tool_name, result_string)
        let mut tasks: Vec<
            std::pin::Pin<
                Box<dyn std::future::Future<Output = (usize, String, String, String)> + Send>,
            >,
        > = Vec::new();

        for (idx, tc) in calls.iter().enumerate() {
            let name = tc.function.name.clone();
            let args: serde_json::Value =
                serde_json::from_str(&tc.function.arguments).unwrap_or(serde_json::Value::Null);

            let args_json = serde_json::to_string(&args).unwrap_or_default();
            let status = format!("\n[[TOOL:{}:{}]]\n", name, args_json);
            let _ = ctx
                .chunk_tx
                .send(IPCResponse::ChatChunk {
                    id: id.to_string(),
                    token: status.clone(),
                })
                .await;
            final_content.push_str(&status);

            let tc_id = tc.id.clone();

            // todo_write runs inline - mutates session state, tiny, no point
            // in async indirection.
            if name == "todo_write" {
                let result = {
                    let todos_val = args
                        .get("todos")
                        .cloned()
                        .unwrap_or(serde_json::Value::Array(vec![]));
                    match serde_json::from_value::<Vec<TodoItem>>(todos_val) {
                        Ok(new_todos) => {
                            let mut summary =
                                format!("Task list updated ({} items):\n", new_todos.len());
                            for (i, t) in new_todos.iter().enumerate() {
                                let marker = match t.status.as_str() {
                                    "completed" => "[x]",
                                    "in_progress" => "[~]",
                                    _ => "[ ]",
                                };
                                summary.push_str(&format!(
                                    "  {}. {} {}\n",
                                    i + 1,
                                    marker,
                                    t.content
                                ));
                            }
                            summary.push_str("\nContinue with the in_progress task. Update statuses as you complete each item.");
                            if let Ok(mut map) = session_todos().lock() {
                                map.insert(conv_id.to_string(), new_todos);
                            }
                            // Push the updated list to the UI so the todo
                            // widget stays in sync with what the agent sees.
                            let _ = ctx
                                .chunk_tx
                                .send(IPCResponse::SessionTodos {
                                    id: id.to_string(),
                                    conversation_id: conv_id.to_string(),
                                    todos: get_session_todos_json(conv_id),
                                })
                                .await;
                            summary
                        }
                        Err(e) => format!("Error: invalid todo list: {}", e),
                    }
                };
                tasks.push(Box::pin(async move {
                    (idx, tc_id, "todo_write".to_string(), result)
                }));
                continue;
            }

            // All other tools: clone what we need and push a future
            let tools_clone = ctx.tools.clone();
            let memory_clone = ctx.memory.clone();
            let llm_clone = effective_llm.clone();
            let cancel_token = ctx.cancel_token.clone();
            let conv_id_owned = conv_id.to_string();
            let chunk_tx = ctx.chunk_tx.clone();
            let id_owned = id.to_string();

            let stream_tx_for_tool = chunk_tx.clone();
            tasks.push(Box::pin(async move {
                let result = tokio::select! {
                    biased;
                    _ = cancel_token.cancelled() => {
                        let _ = chunk_tx.send(IPCResponse::Cancelled {
                            id: id_owned.clone(),
                            target_id: id_owned.clone(),
                        }).await;
                        format!("Error: tool '{}' cancelled", name)
                    }
                    res = tokio::time::timeout(
                        std::time::Duration::from_secs(120),
                        execute_tool(
                            &name, &args,
                            &tools_clone, &memory_clone, &llm_clone,
                            &conv_id_owned,
                            Some(stream_tx_for_tool),
                            &id_owned,
                        ),
                    ) => match res {
                        Ok(r) => r,
                        Err(_) => format!("Error: tool '{}' timed out after 120s. Use run_in_background=true for long commands.", name),
                    }
                };
                (idx, tc_id, name, result)
            }));
        }

        // Fan out in parallel, collect in original order
        let mut results: Vec<(usize, String, String, String)> = join_all(tasks).await;

        // Bail out if cancelled mid-flight
        if ctx.cancel_token.is_cancelled() {
            return Ok(IPCResponse::ChatDone {
                id: id.to_string(),
                conversation_id: conv_id.to_string(),
                usage: None,
            });
        }

        results.sort_by_key(|(idx, _, _, _)| *idx);

        for (_, tc_id, tool_name, result) in results {
            // Emit an error badge in the stream so the UI can visually flag the card
            if result.starts_with("Error:") {
                let _ = ctx
                    .chunk_tx
                    .send(IPCResponse::ChatChunk {
                        id: id.to_string(),
                        token: format!("[[TERR:{}]]", tool_name),
                    })
                    .await;
                final_content.push_str(&format!("[[TERR:{}]]", tool_name));
            }

            let result_trimmed = if result.len() > 12_000 {
                format!(
                    "{}...\n[truncated - {} bytes total]",
                    &result[..12_000],
                    result.len()
                )
            } else {
                result
            };

            messages.push(crate::llm::types::Message {
                role: "tool".to_string(),
                content: result_trimmed,
                tool_call_id: Some(tc_id),
                ..Default::default()
            });
        }
    }

    if !warnings.is_empty() && !final_content.is_empty() {
        final_content = format!(
            "[context guard] {}\n\n{}",
            warnings.join(" "),
            final_content
        );
    }

    persist_assistant_message(&ctx.memory, conv_id, &final_content);

    // code-based extraction from team mode exchanges.
    if !final_content.is_empty() && !user_message_for_distill.is_empty() {
        let mem = ctx.memory.clone();
        let um = user_message_for_distill.clone();
        let ac = final_content.clone();
        tokio::task::spawn_blocking(move || {
            crate::memory::extractor::run(&mem, &um, &ac);
        });
    }

    Ok(IPCResponse::ChatDone {
        id: id.to_string(),
        conversation_id: conv_id.to_string(),
        usage: final_usage,
    })
}

async fn handle_streaming_fallback(
    ctx: &HandlerCtx,
    id: &str,
    conv_id: &str,
    model: Option<&str>,
    mut messages: Vec<crate::llm::types::Message>,
    warnings: Vec<String>,
    curated_struct: Option<serde_json::Value>,
    mode_name: &str,
    effective_llm: &LLMClient,
) -> Result<IPCResponse> {
    let system_text = format!("Agent mode: {}", mode_name);
    messages.insert(0, crate::llm::types::Message::text("system", system_text));

    // Token accounting for debugging context bloat
    let total_chars: usize = messages.iter().map(|m| m.content.len()).sum();
    let est_tokens = total_chars / 4;
    tracing::info!(
        "[chat→{}] dispatching {} messages, ~{} tokens, {} chars",
        mode_name,
        messages.len(),
        est_tokens,
        total_chars
    );
    for (i, m) in messages.iter().enumerate() {
        tracing::info!(
            "  msg[{}] role={} {}chars: {}",
            i,
            m.role,
            m.content.len(),
            m.content
                .chars()
                .take(100)
                .collect::<String>()
                .replace('\n', " ")
        );
    }

    let stream_result = effective_llm
        .chat_stream_for_model(messages.clone(), model)
        .await;

    // Capture rich error context for post-mortem debugging before any fallback
    // logic kicks in. This lands in error.log as a single JSON line.
    if let Err(ref e) = stream_result {
        let est = messages.iter().map(|m| m.content.len()).sum::<usize>() / 4;
        crate::error_log::record(crate::error_log::ErrorContext {
            source: "llm_api::chat_stream_for_model",
            message: &format!("{}", e),
            conversation_id: Some(conv_id),
            model,
            provider: Some(detect_provider(&effective_llm.base_config().base_url)),
            agent_mode: Some(mode_name),
            max_tokens: Some(effective_llm.base_config().max_tokens),
            prompt_preview: Some(crate::error_log::preview_messages(&messages)),
            est_input_tokens: Some(est),
            extra: None,
        });
    }

    match stream_result {
        Ok(Some(resp)) => {
            let mut byte_stream = resp.bytes_stream();
            let mut buffer = String::new();
            let mut full_content = String::new();
            let mut reasoning_buf = String::new(); // accumulate thinking deltas
            let mut thinking_emitted = false;
            let mut cancelled = false;

            loop {
                tokio::select! {
                    biased;
                    _ = ctx.cancel_token.cancelled() => {
                        cancelled = true;
                        break;
                    }
                    chunk_opt = byte_stream.next() => {
                        let chunk_result = match chunk_opt {
                            Some(c) => c,
                            None => break,
                        };
                        let bytes = match chunk_result {
                            Ok(b) => b,
                            Err(e) => { warn!("SSE read error: {}", e); break; }
                        };
                        buffer.push_str(&String::from_utf8_lossy(&bytes));

                        while let Some(pos) = buffer.find('\n') {
                            let line = buffer[..pos].trim_end_matches('\r').to_string();
                            buffer = buffer[pos + 1..].to_string();

                            if line.is_empty() || line.starts_with(':') { continue; }
                            let data = if let Some(rest) = line.strip_prefix("data: ") { rest } else if let Some(rest) = line.strip_prefix("data:") { rest }
                                       else { continue; };
                            if data.trim() == "[DONE]" || data.trim().is_empty() { continue; }

                            if let Ok(chunk) = serde_json::from_str::<crate::llm::types::StreamChunkResponse>(data) {
                                let delta = chunk.choices.first().and_then(|c| c.delta.as_ref());

                                // Collect reasoning/thinking deltas (OpenRouter, Anthropic streaming)
                                if let Some(thought_delta) = delta.and_then(|d| d.thinking_delta()) {
                                    if !thought_delta.is_empty() {
                                        reasoning_buf.push_str(thought_delta);
                                    }
                                }

                                if let Some(token) = delta.and_then(|d| d.content.as_ref()) {
                                    if !token.is_empty() {
                                        // Flush buffered thinking once real content starts arriving
                                        if !thinking_emitted && !reasoning_buf.is_empty() {
                                            let _ = ctx.chunk_tx.send(IPCResponse::ChatThinking {
                                                id: id.to_string(),
                                                thinking: reasoning_buf.clone(),
                                            }).await;
                                            thinking_emitted = true;
                                        }
                                        full_content.push_str(token);
                                        let _ = ctx.chunk_tx.send(IPCResponse::ChatChunk {
                                            id: id.to_string(),
                                            token: token.clone(),
                                        }).await;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // If model only produced reasoning and no text content, still emit thinking
            if !thinking_emitted && !reasoning_buf.is_empty() {
                let _ = ctx
                    .chunk_tx
                    .send(IPCResponse::ChatThinking {
                        id: id.to_string(),
                        thinking: reasoning_buf,
                    })
                    .await;
            }

            drop(byte_stream);

            if cancelled {
                let _ = ctx
                    .chunk_tx
                    .send(IPCResponse::Cancelled {
                        id: id.to_string(),
                        target_id: id.to_string(),
                    })
                    .await;
                return Ok(IPCResponse::ChatDone {
                    id: id.to_string(),
                    conversation_id: conv_id.to_string(),
                    usage: None,
                });
            }

            if full_content.is_empty() && !buffer.trim().is_empty() {
                if let Ok(fallback) =
                    serde_json::from_str::<crate::llm::types::ChatCompletionResponse>(&buffer)
                {
                    full_content = fallback
                        .choices
                        .first()
                        .and_then(|c| c.message.text_content())
                        .unwrap_or_default();
                }
            }

            if !warnings.is_empty() && !full_content.is_empty() {
                full_content =
                    format!("[context guard] {}\n\n{}", warnings.join(" "), full_content);
            }

            persist_assistant_message(&ctx.memory, conv_id, &full_content);

            Ok(IPCResponse::ChatDone {
                id: id.to_string(),
                conversation_id: conv_id.to_string(),
                usage: None,
            })
        }

        Ok(None) | Err(_) => {
            if let Err(ref e) = stream_result {
                warn!("SSE streaming failed ({}), falling back", e);
            }
            let response = effective_llm.chat_for_model(messages, model).await?;

            let mut content = response
                .choices
                .first()
                .and_then(|c| c.message.text_content())
                .unwrap_or_default();

            if !warnings.is_empty() {
                content = format!("[context guard] {}\n\n{}", warnings.join(" "), content);
            }

            let tool_calls = response
                .choices
                .first()
                .and_then(|c| c.message.tool_calls.as_ref())
                .map(|calls| {
                    calls
                        .iter()
                        .map(|tc| ToolCallResult {
                            name: tc.function.name.clone(),
                            args: serde_json::from_str(&tc.function.arguments)
                                .unwrap_or(serde_json::Value::Null),
                            result: String::new(),
                        })
                        .collect()
                });

            persist_assistant_message(&ctx.memory, conv_id, &content);

            Ok(IPCResponse::Chat {
                id: id.to_string(),
                conversation_id: conv_id.to_string(),
                content,
                tool_calls,
                context_packet: curated_struct,
                usage: response.usage.as_ref().map(|u| TokenUsage {
                    prompt_tokens: u.prompt_tokens,
                    completion_tokens: u.completion_tokens,
                    total_tokens: u.total_tokens,
                }),
            })
        }
    }
}

fn persist_assistant_message(
    memory: &crate::memory::storage::MemoryStorage,
    conv_id: &str,
    content: &str,
) {
    if let Ok(conn) = memory.get_connection() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let msg_id = Uuid::new_v4().to_string();
        let _ = conn.execute(
            "INSERT INTO messages (id, conversation_id, role, content, created_at) VALUES (?1, ?2, 'assistant', ?3, ?4)",
            rusqlite::params![&msg_id, conv_id, content, now],
        );
        let _ = conn.execute(
            "UPDATE conversations SET updated_at = ?1 WHERE id = ?2",
            rusqlite::params![now, conv_id],
        );
    }
}

/// Resolve a possibly-relative path against the session cwd for this conversation.
fn resolve_conv_path(conv_id: &str, path: &str) -> std::path::PathBuf {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        get_session_cwd(conv_id).join(p)
    }
}

/// Built-in regex grep - walks files under `root`, applies regex, returns
/// file paths / lines / counts.  Pure Rust, no external ripgrep needed.
fn run_builtin_grep(
    root: &std::path::Path,
    pattern: &str,
    glob_filter: &str,
    output_mode: &str,
    case_i: bool,
    max_results: usize,
) -> String {
    let re = match regex::RegexBuilder::new(pattern)
        .case_insensitive(case_i)
        .build()
    {
        Ok(r) => r,
        Err(e) => return format!("Error: invalid regex: {}", e),
    };

    // Convert a fnmatch-style glob ("*.rs") into a regex. Handles * ? and .
    let glob_re = if glob_filter.is_empty() {
        None
    } else {
        let mut pat = String::from("^");
        for c in glob_filter.chars() {
            match c {
                '*' => pat.push_str(".*"),
                '?' => pat.push('.'),
                '.' => pat.push_str("\\."),
                '(' | ')' | '[' | ']' | '{' | '}' | '+' | '|' | '^' | '$' | '\\' => {
                    pat.push('\\');
                    pat.push(c);
                }
                _ => pat.push(c),
            }
        }
        pat.push('$');
        regex::Regex::new(&pat).ok()
    };

    const IGNORE_DIRS: &[&str] = &[
        ".git",
        "node_modules",
        "target",
        "dist",
        "build",
        ".next",
        ".cache",
        "__pycache__",
        ".venv",
        "venv",
        ".idea",
        ".vscode",
    ];

    let mut files_out: Vec<String> = vec![];
    let mut content_out: Vec<String> = vec![];
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut total_hits = 0usize;

    let walker = walkdir::WalkDir::new(root)
        .follow_links(false)
        .max_depth(16)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !IGNORE_DIRS.contains(&name.as_ref())
        });

    for entry in walker.flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();

        if let Some(ref gre) = glob_re {
            let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !gre.is_match(fname) {
                continue;
            }
        }

        // Skip files larger than 4 MB - almost certainly binary or generated
        if let Ok(meta) = path.metadata() {
            if meta.len() > 4 * 1024 * 1024 {
                continue;
            }
        }

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue, // binary or permission error
        };

        let mut file_hits = 0usize;
        for (lineno, line) in content.lines().enumerate() {
            if re.is_match(line) {
                file_hits += 1;
                total_hits += 1;
                if output_mode == "content" && content_out.len() < max_results {
                    let snippet = if line.len() > 240 {
                        format!("{}...", &line[..240])
                    } else {
                        line.to_string()
                    };
                    content_out.push(format!("{}:{}:{}", path.display(), lineno + 1, snippet));
                }
            }
        }

        if file_hits > 0 {
            match output_mode {
                "files" if files_out.len() < max_results => {
                    files_out.push(path.display().to_string());
                }
                "count" => {
                    counts.insert(path.display().to_string(), file_hits);
                }
                _ => {}
            }
        }

        if total_hits >= max_results * 2 {
            break;
        }
    }

    match output_mode {
        "files" => {
            if files_out.is_empty() {
                "(no matches)".to_string()
            } else {
                format!(
                    "{} file(s) matched:\n{}",
                    files_out.len(),
                    files_out.join("\n")
                )
            }
        }
        "content" => {
            if content_out.is_empty() {
                "(no matches)".to_string()
            } else {
                let mut out = format!(
                    "{} match(es):\n{}",
                    content_out.len(),
                    content_out.join("\n")
                );
                if total_hits > content_out.len() {
                    out.push_str(&format!(
                        "\n... ({} more not shown)",
                        total_hits - content_out.len()
                    ));
                }
                out
            }
        }
        "count" => {
            if counts.is_empty() {
                "(no matches)".to_string()
            } else {
                let mut rows: Vec<(String, usize)> = counts.into_iter().collect();
                rows.sort_by_key(|r| std::cmp::Reverse(r.1));
                rows.iter()
                    .take(max_results)
                    .map(|(f, c)| format!("{}: {}", f, c))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        _ => format!("Unknown output mode: {}", output_mode),
    }
}

/// Extract a symbolic outline (functions, classes, imports) from source code
/// Extract import/use/require statements from source code for dependency context.
fn extract_imports(content: &str, file_path: &str) -> Vec<String> {
    let ext = std::path::Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let mut imports = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        match ext {
            "rs" if trimmed.starts_with("use ")
                || trimmed.starts_with("pub use ")
                || trimmed.starts_with("mod ")
                || trimmed.starts_with("pub mod ") =>
            {
                imports.push(trimmed.trim_end_matches(';').to_string());
            }
            "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => {
                if (trimmed.starts_with("import ") || trimmed.starts_with("export "))
                    && (trimmed.contains(" from ") || trimmed.starts_with("import "))
                {
                    imports.push(trimmed.trim_end_matches(';').to_string());
                }
                if trimmed.starts_with("const ") && trimmed.contains("require(") {
                    imports.push(trimmed.trim_end_matches(';').to_string());
                }
            }
            "py" if trimmed.starts_with("import ") || trimmed.starts_with("from ") => {
                imports.push(trimmed.to_string());
            }
            "go" if trimmed.starts_with("import ")
                || (trimmed.starts_with('"') && trimmed.ends_with('"')) =>
            {
                imports.push(trimmed.to_string());
            }
            _ => {}
        }
    }
    imports
}

/// using language-aware regex heuristics. Much cheaper than parsing the whole
/// file just to find symbol locations.
fn outline_source(path: &std::path::Path, content: &str) -> String {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

    // Patterns: (label, regex). Order matters for output.
    let patterns: Vec<(&str, &str)> = match ext {
        "rs" => vec![
            (
                "fn",
                r"^\s*(?:pub\s+(?:\([^)]*\)\s*)?)?(?:async\s+)?(?:const\s+)?(?:unsafe\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)",
            ),
            (
                "struct",
                r"^\s*(?:pub\s+(?:\([^)]*\)\s*)?)?struct\s+([A-Za-z_][A-Za-z0-9_]*)",
            ),
            (
                "enum",
                r"^\s*(?:pub\s+(?:\([^)]*\)\s*)?)?enum\s+([A-Za-z_][A-Za-z0-9_]*)",
            ),
            (
                "trait",
                r"^\s*(?:pub\s+(?:\([^)]*\)\s*)?)?trait\s+([A-Za-z_][A-Za-z0-9_]*)",
            ),
            (
                "impl",
                r"^\s*impl(?:<[^>]*>)?\s+(?:[A-Za-z_][A-Za-z0-9_:<>, ]*\s+for\s+)?([A-Za-z_][A-Za-z0-9_]*)",
            ),
            ("mod", r"^\s*(?:pub\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)"),
            (
                "use",
                r"^\s*(?:pub\s+)?use\s+([A-Za-z_][A-Za-z0-9_:*{}, ]*)",
            ),
        ],
        "js" | "jsx" | "mjs" | "cjs" => vec![
            (
                "fn",
                r"^\s*(?:export\s+(?:default\s+)?)?(?:async\s+)?function\s+([A-Za-z_$][A-Za-z0-9_$]*)",
            ),
            (
                "class",
                r"^\s*(?:export\s+(?:default\s+)?)?class\s+([A-Za-z_$][A-Za-z0-9_$]*)",
            ),
            (
                "const",
                r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=",
            ),
            ("import", r#"^\s*import\s+(?:.+\s+from\s+)?['""](.+)['""]"#),
        ],
        "ts" | "tsx" => vec![
            (
                "fn",
                r"^\s*(?:export\s+(?:default\s+)?)?(?:async\s+)?function\s+([A-Za-z_$][A-Za-z0-9_$]*)",
            ),
            (
                "class",
                r"^\s*(?:export\s+(?:default\s+)?)?(?:abstract\s+)?class\s+([A-Za-z_$][A-Za-z0-9_$]*)",
            ),
            (
                "interface",
                r"^\s*(?:export\s+)?interface\s+([A-Za-z_$][A-Za-z0-9_$]*)",
            ),
            (
                "type",
                r"^\s*(?:export\s+)?type\s+([A-Za-z_$][A-Za-z0-9_$]*)",
            ),
            (
                "const",
                r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][A-Za-z0-9_$]*)",
            ),
            ("import", r#"^\s*import\s+(?:.+\s+from\s+)?['""](.+)['""]"#),
        ],
        "py" => vec![
            ("def", r"^\s*(?:async\s+)?def\s+([A-Za-z_][A-Za-z0-9_]*)"),
            ("class", r"^\s*class\s+([A-Za-z_][A-Za-z0-9_]*)"),
            (
                "import",
                r"^\s*(?:from\s+([A-Za-z_.][A-Za-z0-9_.]*)\s+)?import\s",
            ),
        ],
        "go" => vec![
            (
                "func",
                r"^\s*func\s+(?:\([^)]+\)\s+)?([A-Za-z_][A-Za-z0-9_]*)",
            ),
            ("type", r"^\s*type\s+([A-Za-z_][A-Za-z0-9_]*)"),
            ("import", r#"^\s*import\s+['""](.+)['""]"#),
        ],
        _ => vec![
            // Generic fallback: markdown headers + top-level comments
            ("header", r"^(#{1,6})\s+(.+)$"),
        ],
    };

    let compiled: Vec<(&str, regex::Regex)> = patterns
        .into_iter()
        .filter_map(|(label, pat)| regex::Regex::new(pat).ok().map(|re| (label, re)))
        .collect();

    let mut hits: Vec<(usize, String, String)> = vec![];
    for (lineno, line) in content.lines().enumerate() {
        for (label, re) in &compiled {
            if let Some(caps) = re.captures(line) {
                let name = caps
                    .get(1)
                    .or_else(|| caps.get(2))
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default();
                if !name.is_empty() {
                    hits.push((lineno + 1, label.to_string(), name));
                }
                break;
            }
        }
    }

    if hits.is_empty() {
        return format!(
            "No symbols extracted from {} (unsupported or empty file).",
            path.display()
        );
    }

    let mut out = format!("Outline of {} ({} symbols):\n", path.display(), hits.len());
    for (ln, label, name) in hits.iter().take(400) {
        out.push_str(&format!("  {:>5}  {:>8}  {}\n", ln, label, name));
    }
    if hits.len() > 400 {
        out.push_str(&format!("  ... ({} more)\n", hits.len() - 400));
    }
    out
}

/// Auto-index a file that was just read/written/edited into the memory graph.
/// Creates a File node (or skips if one already exists for this path) so it
/// becomes discoverable in future memory searches.  Non-fatal on failure.
fn auto_index_file(
    memory: &crate::memory::storage::MemoryStorage,
    conv_id: &str,
    abs_path: &str,
    action: &str,
) {
    // Deduplicate: skip if we already have a File node for this exact path.
    // Use a direct SQL query rather than the graph search API (which is fuzzy).
    if let Ok(conn) = memory.get_connection() {
        let already_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM nodes WHERE node_type = 'file' \
             AND metadata_json LIKE ?1 LIMIT 1)",
                rusqlite::params![format!("%\"path\":\"{}%", abs_path)],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if already_exists {
            return;
        }
    }

    let graph = crate::memory::graph::GraphMemory::new(memory.clone());
    let filename = std::path::Path::new(abs_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(abs_path);
    let title = format!("{} ({})", filename, action);
    let meta = serde_json::json!({
        "path": abs_path,
        "action": action,
        "conversation_id": conv_id,
        "source": "auto_index",
    });
    let _ = graph.create_node(crate::memory::graph::NodeType::File, &title, Some(meta));
}

/// Run `git diff HEAD -- <path>` for a file and return a formatted prefix if
/// there are uncommitted changes.  Returns `None` if the file is clean, the
/// path isn't in a git repo, or git isn't available.
fn git_diff_for_file(abs_path: &str) -> Option<String> {
    let parent = std::path::Path::new(abs_path).parent()?;
    let mut cmd = std::process::Command::new("git");
    let output = crate::os::hide(&mut cmd)
        .args(["diff", "HEAD", "--", abs_path])
        .current_dir(parent)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let diff = String::from_utf8_lossy(&output.stdout);
    let trimmed = diff.trim();
    if trimmed.is_empty() {
        // Also check working-tree-only changes (untracked diff against index)
        let mut cmd2 = std::process::Command::new("git");
        let output2 = crate::os::hide(&mut cmd2)
            .args(["diff", "--", abs_path])
            .current_dir(parent)
            .output()
            .ok()?;
        let diff2 = String::from_utf8_lossy(&output2.stdout);
        let trimmed2 = diff2.trim();
        if trimmed2.is_empty() {
            return None;
        }
        // Truncate large diffs to stay within context budget
        let snippet: String = trimmed2.chars().take(3_000).collect();
        return Some(format!(
            "[Recent uncommitted changes]\n```diff\n{}\n```\n",
            snippet
        ));
    }
    let snippet: String = trimmed.chars().take(3_000).collect();
    Some(format!(
        "[Recent uncommitted changes]\n```diff\n{}\n```\n",
        snippet
    ))
}

/// Apply a unified diff patch to file content.
/// Handles standard `@@` hunks; context lines are used for alignment but not
/// strictly verified (suitable for LLM-generated patches which are usually correct).
fn apply_unified_patch(original: &str, patch: &str) -> anyhow::Result<String> {
    let ends_with_nl = original.ends_with('\n');
    let orig_lines: Vec<&str> = original.lines().collect();
    let patch_lines: Vec<&str> = patch.lines().collect();
    let hunk_re = regex::Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")?;

    let mut result: Vec<String> = orig_lines.iter().map(|s| s.to_string()).collect();
    let mut i = 0;

    // Skip diff header lines (---, +++, diff, index)
    while i < patch_lines.len() {
        let l = patch_lines[i];
        if l.starts_with("---")
            || l.starts_with("+++")
            || l.starts_with("diff ")
            || l.starts_with("index ")
        {
            i += 1;
        } else {
            break;
        }
    }

    let mut offset: i64 = 0; // cumulative line delta from applied hunks

    while i < patch_lines.len() {
        let line = patch_lines[i];
        if let Some(caps) = hunk_re.captures(line) {
            let orig_start: i64 = caps[1].parse::<i64>()?.saturating_sub(1); // 0-based

            // Collect hunk body (stop at next @@ or end)
            let mut hunk: Vec<&str> = vec![];
            i += 1;
            while i < patch_lines.len() && !patch_lines[i].starts_with("@@") {
                let hl = patch_lines[i];
                // Skip "\ No newline at end of file" markers
                if !hl.starts_with('\\') {
                    hunk.push(hl);
                }
                i += 1;
            }

            let apply_at = (orig_start + offset).max(0) as usize;
            let mut to_remove = 0usize;
            let mut to_add: Vec<String> = vec![];

            for hl in &hunk {
                if hl.is_empty() {
                    // Bare empty line in hunk = context (blank line)
                    to_remove += 1;
                    to_add.push(String::new());
                    continue;
                }
                match &hl[..1] {
                    " " => {
                        to_remove += 1;
                        to_add.push(hl[1..].to_string());
                    }
                    "-" => {
                        to_remove += 1;
                    }
                    "+" => {
                        to_add.push(hl[1..].to_string());
                    }
                    _ => {}
                }
            }

            let end = (apply_at + to_remove).min(result.len());
            let diff = to_add.len() as i64 - to_remove as i64;
            result.splice(apply_at..end, to_add);
            offset += diff;
        } else {
            i += 1;
        }
    }

    let mut out = result.join("\n");
    if ends_with_nl && !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

// Execute a single tool call by name and return the result as a string.
pub async fn execute_tool(
    name: &str,
    args: &serde_json::Value,
    tools: &crate::tools::ToolExecutor,
    memory: &crate::memory::storage::MemoryStorage,
    llm: &LLMClient,
    conv_id: &str,
    stream_tx: Option<tokio::sync::mpsc::Sender<IPCResponse>>,
    stream_id: &str,
) -> String {
    let s = |key: &str| args.get(key).and_then(|v| v.as_str()).unwrap_or("");
    match name {
        "search_memory" => {
            let query = s("query");
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .unwrap_or(5)
                .min(15);
            if query.trim().is_empty() {
                return "search_memory: query is required".to_string();
            }
            let curator = crate::memory::context::ContextCurator::new(memory.clone());
            match curator
                .curate_for_query(llm, query, Some(limit * 4), Some(limit))
                .await
            {
                Ok(hits) if !hits.is_empty() => {
                    let compact: Vec<serde_json::Value> = hits
                        .iter()
                        .map(|h| {
                            serde_json::json!({
                                "title": h.title,
                                "summary": h.summary.clone().unwrap_or_default(),
                                "confidence": h.confidence_score,
                                "node_id": h.node_id,
                            })
                        })
                        .collect();
                    serde_json::to_string_pretty(&compact).unwrap_or_default()
                }
                _ => format!("search_memory: no results for query '{}'", query),
            }
        }
        "file_read" => {
            let path = s("path");
            let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize);
            let abs = resolve_conv_path(conv_id, path);
            let abs_str = abs.display().to_string();
            match tools.read_file(&abs_str).await {
                Ok(c) => {
                    // Take a snapshot on first read so file_diff / file_undo work
                    snapshot_file(conv_id, &abs_str);
                    auto_index_file(memory, conv_id, &abs_str, "read");

                    // Check for uncommitted git changes to this file - prepend
                    // the diff so the model sees what changed without needing to
                    // compare against stale indexed content.
                    let diff_prefix = git_diff_for_file(&abs_str);

                    // Return line-numbered content (cat -n format) so the model can
                    // reference exact line numbers in subsequent edits.
                    let lines: Vec<&str> = c.lines().collect();
                    let total = lines.len();
                    let start = offset.min(total);
                    let end = match limit {
                        Some(n) => (start + n).min(total),
                        None => total.min(start + 2000),
                    };
                    let mut out = String::new();
                    if let Some(diff) = diff_prefix {
                        out.push_str(&diff);
                        out.push('\n');
                    }
                    for (i, line) in lines[start..end].iter().enumerate() {
                        out.push_str(&format!("{:6}\t{}\n", start + i + 1, line));
                    }
                    if end < total {
                        out.push_str(&format!(
                            "\n[file has {} lines total - pass offset/limit to read more]",
                            total
                        ));
                    }

                    // Auto-extract imports/dependencies for context
                    let imports = extract_imports(&c, &abs_str);
                    if !imports.is_empty() {
                        out.push_str("\n[Imports/dependencies detected:\n");
                        for imp in imports.iter().take(20) {
                            out.push_str(&format!("  {}\n", imp));
                        }
                        out.push_str("]\n");
                    }

                    out
                }
                Err(e) => format!("Error: {}", e),
            }
        }
        "list_files" => {
            let raw_path = s("path");
            let target = if raw_path.is_empty() {
                ".".to_string()
            } else {
                raw_path.to_string()
            };
            let recursive = args
                .get("recursive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let resolved = resolve_conv_path(conv_id, &target);
            let base = match std::fs::canonicalize(&resolved) {
                Ok(p) => p,
                Err(e) => return format!("Error: cannot resolve '{}': {}", resolved.display(), e),
            };
            if !base.is_dir() {
                return format!("Error: '{}' is not a directory", base.display());
            }
            let mut out = format!("Listing {} (recursive={})\n", base.display(), recursive);
            let max_entries = 500usize;
            let max_depth = if recursive { 3 } else { 1 };
            let mut count = 0usize;
            fn walk(
                dir: &std::path::Path,
                base: &std::path::Path,
                depth: usize,
                max_depth: usize,
                pattern: &str,
                out: &mut String,
                count: &mut usize,
                max: usize,
            ) {
                if *count >= max {
                    return;
                }
                let entries = match std::fs::read_dir(dir) {
                    Ok(e) => e,
                    Err(e) => {
                        out.push_str(&format!("  [error: {}]\n", e));
                        return;
                    }
                };
                let mut items: Vec<_> = entries.flatten().collect();
                items.sort_by_key(|e| e.file_name());
                for entry in items {
                    if *count >= max {
                        out.push_str(&format!("  [... truncated at {} entries]\n", max));
                        return;
                    }
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !pattern.is_empty() && !name.to_lowercase().contains(&pattern.to_lowercase())
                    {
                        continue;
                    }
                    let ft = entry.file_type().ok();
                    let is_dir = ft.as_ref().map(|t| t.is_dir()).unwrap_or(false);
                    let rel = entry
                        .path()
                        .strip_prefix(base)
                        .ok()
                        .map(|p| p.display().to_string())
                        .unwrap_or(name.clone());
                    let indent = "  ".repeat(depth);
                    if is_dir {
                        out.push_str(&format!("{}{}/  (dir)\n", indent, rel));
                        *count += 1;
                        if depth + 1 < max_depth {
                            walk(
                                &entry.path(),
                                base,
                                depth + 1,
                                max_depth,
                                pattern,
                                out,
                                count,
                                max,
                            );
                        }
                    } else {
                        let size = entry.metadata().ok().map(|m| m.len()).unwrap_or(0);
                        out.push_str(&format!("{}{}  ({} bytes)\n", indent, rel, size));
                        *count += 1;
                    }
                }
            }
            walk(
                &base,
                &base,
                0,
                max_depth,
                pattern,
                &mut out,
                &mut count,
                max_entries,
            );
            if count == 0 {
                out.push_str("  (empty)\n");
            }
            out
        }
        "change_dir" => {
            let target = s("path");
            if target.is_empty() {
                return "Error: path is required".to_string();
            }
            let resolved = resolve_conv_path(conv_id, target);
            match std::fs::canonicalize(&resolved) {
                Ok(p) if p.is_dir() => {
                    set_session_cwd(conv_id, p.clone());
                    // Spawn background neural indexing so every file in the working
                    // directory becomes semantically searchable via the memory system.
                    let mem_clone = memory.clone();
                    let llm_clone = llm.clone();
                    let dir_clone = p.clone();
                    tokio::spawn(async move {
                        let indexer = crate::indexer::file_indexer::FileIndexer::new_with_llm(
                            mem_clone, llm_clone,
                        );
                        match indexer.index_directory(&dir_clone).await {
                            Ok(n) => tracing::info!(
                                "[RAG] Indexed {} files under {}",
                                n,
                                dir_clone.display()
                            ),
                            Err(e) => tracing::warn!(
                                "[RAG] Indexing failed for {}: {}",
                                dir_clone.display(),
                                e
                            ),
                        }
                    });
                    format!(
                        "Changed directory to: {} - indexing files for semantic search.",
                        p.display()
                    )
                }
                Ok(p) => format!("Error: '{}' is not a directory", p.display()),
                Err(e) => format!("Error: cannot change to '{}': {}", resolved.display(), e),
            }
        }
        "get_cwd" => get_session_cwd(conv_id).display().to_string(),
        "file_write" => {
            let path = s("path");
            let abs = resolve_conv_path(conv_id, path);
            let abs_str = abs.display().to_string();
            // Snapshot previous content BEFORE overwriting
            snapshot_file(conv_id, &abs_str);
            match tools.write_file(&abs_str, s("content")).await {
                Ok(_) => {
                    auto_index_file(memory, conv_id, &abs_str, "write");
                    format!("Wrote {} ({} bytes)", abs_str, s("content").len())
                }
                Err(e) => format!("Error: {}", e),
            }
        }
        "code_edit" => {
            let action = s("action");
            let path = s("path");
            let abs = resolve_conv_path(conv_id, path);
            let abs_str = abs.display().to_string();
            // Snapshot BEFORE the edit so file_undo / file_diff are useful
            snapshot_file(conv_id, &abs_str);
            let result = match action {
                "search_replace" => {
                    tools
                        .code_search_replace(&abs_str, s("search"), s("replace"))
                        .await
                }
                "insert_at_line" => {
                    let line = args.get("line").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
                    tools
                        .code_insert_at_line(&abs_str, line, s("content"))
                        .await
                }
                "append" => tools.code_append(&abs_str, s("content")).await,
                other => Err(anyhow::anyhow!("Unknown code_edit action: {}", other)),
            };
            match result {
                Ok(r) => {
                    auto_index_file(memory, conv_id, &abs_str, "edit");
                    r
                }
                Err(e) => format!("Error: {}", e),
            }
        }
        "terminal_execute" => {
            // Route through the shell manager so cwd persists, background
            // commands are supported, and the 'default' shell stays alive
            // for quick one-offs.
            let shell_name = args
                .get("shell")
                .and_then(|v| v.as_str())
                .unwrap_or("default");
            let background = args
                .get("run_in_background")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let timeout_secs = args.get("timeout_secs").and_then(|v| v.as_u64());
            let command = s("command");
            if command.is_empty() {
                return "Error: command is required".to_string();
            }
            // Seed the shell with the session cwd if it's a fresh spawn
            let sess_cwd = get_session_cwd(conv_id);
            let _ = tools
                .shells
                .execute(
                    shell_name,
                    &format!("cd \"{}\"", sess_cwd.display()),
                    false,
                    Some(2),
                )
                .await;
            if background {
                match tools
                    .shells
                    .execute(shell_name, command, true, timeout_secs)
                    .await
                {
                    Ok(out) => out,
                    Err(e) => format!("Error: {}", e),
                }
            } else if let Some(tx) = stream_tx.clone() {
                // Streamed execution: every stdout line is forwarded to the UI
                // as a ChatChunk so the user sees build output live.
                let id_owned = stream_id.to_string();
                let shell_label = shell_name.to_string();
                let result = tools
                    .shells
                    .execute_streaming(
                        shell_name,
                        command,
                        move |line| {
                            let marker = format!("\n```sh [{}]\n{}\n```", shell_label, line);
                            let _ = tx.try_send(IPCResponse::ChatChunk {
                                id: id_owned.clone(),
                                token: marker,
                            });
                        },
                        timeout_secs,
                    )
                    .await;
                match result {
                    Ok(out) => out,
                    Err(e) => format!("Error: {}", e),
                }
            } else {
                match tools
                    .shells
                    .execute(shell_name, command, false, timeout_secs)
                    .await
                {
                    Ok(out) => out,
                    Err(e) => format!("Error: {}", e),
                }
            }
        }
        "glob" => {
            let pattern = s("pattern");
            if pattern.is_empty() {
                return "Error: pattern is required".to_string();
            }
            let base = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            let resolved = resolve_conv_path(conv_id, base);
            let base_path = match std::fs::canonicalize(&resolved) {
                Ok(p) => p,
                Err(e) => return format!("Error: cannot resolve '{}': {}", resolved.display(), e),
            };
            // Build full pattern: if pattern is relative, prepend base
            let full = if std::path::Path::new(pattern).is_absolute() {
                pattern.to_string()
            } else {
                format!("{}/{}", base_path.display(), pattern)
            };
            match glob::glob(&full) {
                Ok(paths) => {
                    let mut entries: Vec<(std::path::PathBuf, std::time::SystemTime)> = paths
                        .flatten()
                        .filter_map(|p| {
                            let mtime = std::fs::metadata(&p).and_then(|m| m.modified()).ok()?;
                            Some((p, mtime))
                        })
                        .collect();
                    entries.sort_by_key(|e| std::cmp::Reverse(e.1));
                    if entries.is_empty() {
                        format!("No files matched pattern '{}'", pattern)
                    } else {
                        let mut out = format!("{} matches for '{}':\n", entries.len(), pattern);
                        for (p, _) in entries.iter().take(200) {
                            out.push_str(&format!("{}\n", p.display()));
                        }
                        if entries.len() > 200 {
                            out.push_str(&format!("... ({} more)\n", entries.len() - 200));
                        }
                        out
                    }
                }
                Err(e) => format!("Error: invalid glob pattern: {}", e),
            }
        }
        "grep" => {
            let pattern = s("pattern");
            if pattern.is_empty() {
                return "Error: pattern is required".to_string();
            }
            let path_arg = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            let glob_filter = args.get("glob").and_then(|v| v.as_str()).unwrap_or("");
            let output_mode = args
                .get("output")
                .and_then(|v| v.as_str())
                .unwrap_or("files");
            let case_i = args
                .get("case_insensitive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let max = args
                .get("max_results")
                .and_then(|v| v.as_u64())
                .unwrap_or(200) as usize;
            let root = resolve_conv_path(conv_id, path_arg);
            run_builtin_grep(&root, pattern, glob_filter, output_mode, case_i, max)
        }
        "search_in_file" => {
            let path = s("path");
            let pattern = s("pattern");
            if path.is_empty() || pattern.is_empty() {
                return "Error: path and pattern are required".to_string();
            }
            let abs = resolve_conv_path(conv_id, path);
            let context = args.get("context").and_then(|v| v.as_u64()).unwrap_or(2) as usize;
            let case_i = args
                .get("case_insensitive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            match std::fs::read_to_string(&abs) {
                Ok(content) => {
                    let re = match regex::RegexBuilder::new(pattern)
                        .case_insensitive(case_i)
                        .build()
                    {
                        Ok(r) => r,
                        Err(e) => return format!("Error: invalid regex: {}", e),
                    };
                    let lines: Vec<&str> = content.lines().collect();
                    let mut hits: Vec<String> = vec![];
                    for (i, line) in lines.iter().enumerate() {
                        if re.is_match(line) {
                            let start = i.saturating_sub(context);
                            let end = (i + context + 1).min(lines.len());
                            let block: Vec<String> = (start..end)
                                .map(|j| {
                                    let marker = if j == i { ">" } else { " " };
                                    format!("{} {:>5} {}", marker, j + 1, lines[j])
                                })
                                .collect();
                            hits.push(block.join("\n"));
                        }
                    }
                    if hits.is_empty() {
                        format!("No matches for '{}' in {}", pattern, abs.display())
                    } else {
                        format!(
                            "{} matches in {}:\n\n{}",
                            hits.len(),
                            abs.display(),
                            hits.join("\n---\n")
                        )
                    }
                }
                Err(e) => format!("Error reading '{}': {}", abs.display(), e),
            }
        }
        "outline_file" => {
            let path = s("path");
            if path.is_empty() {
                return "Error: path is required".to_string();
            }
            let abs = resolve_conv_path(conv_id, path);
            match std::fs::read_to_string(&abs) {
                Ok(content) => outline_source(&abs, &content),
                Err(e) => format!("Error reading '{}': {}", abs.display(), e),
            }
        }
        "file_diff" => {
            let path = s("path");
            if path.is_empty() {
                return "Error: path is required".to_string();
            }
            let abs = resolve_conv_path(conv_id, path);
            let current = match std::fs::read_to_string(&abs) {
                Ok(c) => c,
                Err(e) => return format!("Error reading current file: {}", e),
            };
            match get_snapshot(conv_id, &abs.display().to_string()) {
                Some(prev) => {
                    if prev == current {
                        format!("No changes to {} in this conversation.", abs.display())
                    } else {
                        crate::tools::ToolExecutor::compute_diff(&prev, &current, &abs.display().to_string())
                    }
                }
                None => format!("No snapshot for {}: no edits have been made to this file yet in this conversation.", abs.display()),
            }
        }
        "file_undo" => {
            let path = s("path");
            if path.is_empty() {
                return "Error: path is required".to_string();
            }
            let abs = resolve_conv_path(conv_id, path);
            let abs_str = abs.display().to_string();
            match pop_snapshot(conv_id, &abs_str) {
                Some(prev) => match std::fs::write(&abs, &prev) {
                    Ok(_) => {
                        let remaining = undo_depth(conv_id, &abs_str);
                        format!(
                            "Restored {} ({} bytes). {} undo level{} remaining.",
                            abs_str,
                            prev.len(),
                            remaining,
                            if remaining == 1 { "" } else { "s" }
                        )
                    }
                    Err(e) => format!("Error: {}", e),
                },
                None => format!("No undo history for {}.", abs_str),
            }
        }
        "apply_patch" => {
            let path = s("path");
            let patch_text = s("patch");
            if path.is_empty() || patch_text.is_empty() {
                return "Error: both path and patch are required".to_string();
            }
            let abs = resolve_conv_path(conv_id, path);
            let abs_str = abs.display().to_string();
            snapshot_file(conv_id, &abs_str);
            match std::fs::read_to_string(&abs) {
                Ok(original) => match apply_unified_patch(&original, patch_text) {
                    Ok(patched) => match std::fs::write(&abs, &patched) {
                        Ok(_) => {
                            auto_index_file(memory, conv_id, &abs_str, "patched");
                            format!(
                                "Applied patch to {} ({} → {} bytes)",
                                abs_str,
                                original.len(),
                                patched.len()
                            )
                        }
                        Err(e) => format!("Error writing patched file: {}", e),
                    },
                    Err(e) => format!("Error: patch failed: {}", e),
                },
                Err(e) => format!("Error reading '{}': {}", abs_str, e),
            }
        }
        "shell_spawn" => {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("shell1");
            match tools.shells.spawn(name).await {
                Ok(r) => r,
                Err(e) => format!("Error: {}", e),
            }
        }
        "shell_exec" => {
            let shell_name = args
                .get("shell")
                .and_then(|v| v.as_str())
                .unwrap_or("default");
            let command = s("command");
            let background = args
                .get("run_in_background")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let timeout_secs = args.get("timeout_secs").and_then(|v| v.as_u64());
            if command.is_empty() {
                return "Error: command is required".to_string();
            }
            if background {
                match tools
                    .shells
                    .execute(shell_name, command, true, timeout_secs)
                    .await
                {
                    Ok(r) => r,
                    Err(e) => format!("Error: {}", e),
                }
            } else if let Some(tx) = stream_tx.clone() {
                let id_owned = stream_id.to_string();
                let shell_label = shell_name.to_string();
                let result = tools
                    .shells
                    .execute_streaming(
                        shell_name,
                        command,
                        move |line| {
                            let marker = format!("\n```sh [{}]\n{}\n```", shell_label, line);
                            let _ = tx.try_send(IPCResponse::ChatChunk {
                                id: id_owned.clone(),
                                token: marker,
                            });
                        },
                        timeout_secs,
                    )
                    .await;
                match result {
                    Ok(r) => r,
                    Err(e) => format!("Error: {}", e),
                }
            } else {
                match tools
                    .shells
                    .execute(shell_name, command, false, timeout_secs)
                    .await
                {
                    Ok(r) => r,
                    Err(e) => format!("Error: {}", e),
                }
            }
        }
        "shell_read" => {
            let name = args
                .get("shell")
                .and_then(|v| v.as_str())
                .unwrap_or("default");
            let clear = args.get("clear").and_then(|v| v.as_bool()).unwrap_or(false);
            match tools.shells.read_output(name, clear).await {
                Ok(r) => r,
                Err(e) => format!("Error: {}", e),
            }
        }
        "shell_kill" => {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("default");
            match tools.shells.kill(name).await {
                Ok(r) => r,
                Err(e) => format!("Error: {}", e),
            }
        }
        "shell_list" => tools.shells.list().await,
        "git_status" => tools
            .git_status(None)
            .await
            .unwrap_or_else(|e| format!("Error: {}", e)),
        "git_diff" => {
            let file = args.get("file").and_then(|v| v.as_str());
            tools
                .git_diff(file)
                .await
                .unwrap_or_else(|e| format!("Error: {}", e))
        }
        "git_log" => {
            let n = args.get("count").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
            tools
                .git_log(n)
                .await
                .unwrap_or_else(|e| format!("Error: {}", e))
        }
        "git_branch" => tools
            .git_branch()
            .await
            .unwrap_or_else(|e| format!("Error: {}", e)),
        "lsp_definition" => {
            let path = s("path");
            let line = args.get("line").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let character = args.get("character").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            if path.is_empty() {
                return "Error: path is required".to_string();
            }
            let abs = resolve_conv_path(conv_id, path);
            tools
                .lsp
                .go_to_definition(&abs.display().to_string(), line, character)
                .unwrap_or_else(|e| format!("LSP error: {}", e))
        }
        "lsp_references" => {
            let path = s("path");
            let line = args.get("line").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let character = args.get("character").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            if path.is_empty() {
                return "Error: path is required".to_string();
            }
            let abs = resolve_conv_path(conv_id, path);
            tools
                .lsp
                .find_references(&abs.display().to_string(), line, character)
                .unwrap_or_else(|e| format!("LSP error: {}", e))
        }
        "lsp_hover" => {
            let path = s("path");
            let line = args.get("line").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let character = args.get("character").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            if path.is_empty() {
                return "Error: path is required".to_string();
            }
            let abs = resolve_conv_path(conv_id, path);
            tools
                .lsp
                .hover(&abs.display().to_string(), line, character)
                .unwrap_or_else(|e| format!("LSP error: {}", e))
        }
        "git_commit" => {
            let message = s("message");
            if message.is_empty() {
                return "Error: message is required".to_string();
            }
            let files: Vec<String> = args
                .get("files")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_else(|| vec![".".to_string()]);
            let file_refs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
            tools
                .git_commit(message, &file_refs)
                .await
                .unwrap_or_else(|e| format!("Error: {}", e))
        }
        "web_search" => match tools.web_search(s("query")).await {
            Ok(results) => serde_json::to_string_pretty(&results).unwrap_or_default(),
            Err(e) => format!("Error: {}", e),
        },
        "fetch_url" => match tools.fetch_url(s("url")).await {
            Ok(text) => text,
            Err(e) => format!("Error: {}", e),
        },
        "browser_navigate" => {
            let _ = tools.spawn_cdp_browser(true).await;
            match tools.cdp_navigate(s("url")).await {
                Ok(text) => text,
                Err(e) => format!("Error: {}", e),
            }
        }
        "browser_click" => match tools.cdp_click(s("selector")).await {
            Ok(_) => "Clicked".to_string(),
            Err(e) => format!("Error: {}", e),
        },
        "browser_type" => match tools.cdp_type(s("selector"), s("text")).await {
            Ok(_) => "Typed".to_string(),
            Err(e) => format!("Error: {}", e),
        },
        "browser_evaluate" => match tools.cdp_evaluate(s("js")).await {
            Ok(r) => r,
            Err(e) => format!("Error: {}", e),
        },
        "browser_screenshot" => {
            let full = args
                .get("full_page")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            match tools.cdp_screenshot_base64(full).await {
                Ok(b64) => format!("[screenshot: {} bytes base64]", b64.len()),
                Err(e) => format!("Error: {}", e),
            }
        }
        "store_memory" => {
            let content = s("content");
            let category = args
                .get("category")
                .and_then(|v| v.as_str())
                .unwrap_or("general");
            if content.is_empty() {
                return "Error: content is required".to_string();
            }
            let graph = crate::memory::graph::GraphMemory::new(memory.clone());
            let title = if content.len() > 120 {
                format!("{}…", &content[..120])
            } else {
                content.to_string()
            };
            let meta = serde_json::json!({ "category": category, "source": "llm_tool" });
            match graph.create_node(crate::memory::graph::NodeType::Concept, &title, Some(meta)) {
                Ok(node) => {
                    if let Ok(vec) = llm.get_embedding(content).await {
                        let emb = crate::memory::embedding::EmbeddingMemory::new(memory.clone());
                        let _ = emb.store(
                            &node.id,
                            crate::memory::embedding::EmbeddingType::Summary,
                            &vec,
                            Some(content),
                        );
                    }
                    let obj_store = crate::memory::object::ObjectMemoryStore::new(memory.clone());
                    let obj = crate::memory::object::ObjectMemory {
                        id: Uuid::new_v4().to_string(),
                        node_id: node.id.clone(),
                        summary: Some(content.to_string()),
                        key_facts: vec![content.to_string()],
                        extracted_structure: None,
                        code_signatures: None,
                        todos: vec![],
                        ui_snapshot: None,
                        tags: vec![category.to_string()],
                        content_hash: None,
                        last_indexed: 0,
                        access_count: 0,
                    };
                    let _ = obj_store.upsert(&node.id, &obj);
                    format!("Stored memory: {}", title)
                }
                Err(e) => format!("Error storing memory: {}", e),
            }
        }
        "store_user_fact" => {
            let key = s("key");
            let value = s("value");
            if key.is_empty() || value.is_empty() {
                return "Error: both key and value are required".to_string();
            }
            match memory.get_connection() {
                Ok(conn) => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    match conn.execute(
                        "INSERT INTO user_facts (key, value, created_at, updated_at) VALUES (?1, ?2, ?3, ?3) \
                         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                        rusqlite::params![key, value, now],
                    ) {
                        Ok(_) => format!("Stored user fact: {} = {}", key, value),
                        Err(e) => format!("Error storing user fact: {}", e),
                    }
                }
                Err(e) => format!("Error: {}", e),
            }
        }
        // ---------- computer use (windows only) ----------
        "ui_snapshot" => {
            let window = s("window");
            let include_offscreen = args
                .get("include_offscreen")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let tree = if window.trim().is_empty() {
                crate::computer_use::uia::snapshot_foreground(include_offscreen)
            } else {
                crate::computer_use::uia::snapshot_window(window)
            };
            let record_ok = tree.is_ok();
            let body = match &tree {
                Ok(v) => serde_json::to_string_pretty(v).unwrap_or_default(),
                Err(e) => format!("ui_snapshot failed: {}", e),
            };
            if record_ok {
                crate::computer_use::context::set_last_tree(conv_id, &body);
            }
            crate::computer_use::context::record(
                conv_id,
                crate::computer_use::ActionRecord {
                    at: chrono::Utc::now().timestamp(),
                    kind: "snapshot".to_string(),
                    target: if window.is_empty() {
                        "<foreground>".into()
                    } else {
                        window.to_string()
                    },
                    value: None,
                    ok: record_ok,
                    error: if record_ok { None } else { Some(body.clone()) },
                },
            );
            let history = crate::computer_use::context::compact_summary(conv_id, 8);
            if history.is_empty() {
                body
            } else {
                format!("{}\n\n# Recent actions\n{}", body, history)
            }
        }
        "ui_click" => {
            let target = s("element_id");
            let result = crate::computer_use::actions::click(target);
            let ok = result.is_ok();
            let err = result.as_ref().err().map(|e| e.to_string());
            crate::computer_use::context::record(
                conv_id,
                crate::computer_use::ActionRecord {
                    at: chrono::Utc::now().timestamp(),
                    kind: "click".into(),
                    target: target.to_string(),
                    value: None,
                    ok,
                    error: err.clone(),
                },
            );
            match result {
                Ok(()) => format!("clicked {}", target),
                Err(e) => format!("ui_click failed: {}", e),
            }
        }
        "ui_type" => {
            let target = s("element_id");
            let text = s("text");
            let result = crate::computer_use::actions::type_text(target, text);
            let ok = result.is_ok();
            let err = result.as_ref().err().map(|e| e.to_string());
            crate::computer_use::context::record(
                conv_id,
                crate::computer_use::ActionRecord {
                    at: chrono::Utc::now().timestamp(),
                    kind: "type".into(),
                    target: target.to_string(),
                    value: Some(text.chars().take(60).collect()),
                    ok,
                    error: err.clone(),
                },
            );
            match result {
                Ok(()) => format!("typed into {}", target),
                Err(e) => format!("ui_type failed: {}", e),
            }
        }
        "ui_focus_window" => {
            let title = s("title");
            let result = crate::computer_use::actions::focus_window(title);
            let ok = result.is_ok();
            let err = result.as_ref().err().map(|e| e.to_string());
            crate::computer_use::context::record(
                conv_id,
                crate::computer_use::ActionRecord {
                    at: chrono::Utc::now().timestamp(),
                    kind: "focus".into(),
                    target: title.to_string(),
                    value: None,
                    ok,
                    error: err.clone(),
                },
            );
            match result {
                Ok(()) => format!("focused {}", title),
                Err(e) => format!("ui_focus_window failed: {}", e),
            }
        }
        "ui_find" => {
            let query = s("query");
            match crate::computer_use::uia::find_element(query) {
                Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_default(),
                Err(e) => format!("ui_find failed: {}", e),
            }
        }

        // ---------- scheduler ----------
        "schedule_task" | "propose_schedule" => {
            let name = s("name");
            let cadence = s("cadence");
            let prompt = s("prompt");
            let channel = args
                .get("output_channel")
                .and_then(|v| v.as_str())
                .unwrap_or("notification")
                .to_string();
            let why = args.get("why").and_then(|v| v.as_str()).map(str::to_string);
            if name.is_empty() || cadence.is_empty() || prompt.is_empty() {
                return "schedule_task: name, cadence, and prompt are required".to_string();
            }
            match crate::scheduler::cadence::parse(cadence, chrono::Local::now()) {
                Ok((next, _kind)) => {
                    // both "schedule_task" and "propose_schedule" from the AI are
                    // treated as AI-sourced for permission gating. user-initiated
                    // schedules come in via the IPC path, not here.
                    let source = crate::scheduler::TaskSource::Ai;
                    let task = crate::scheduler::store::new_task(
                        name.to_string(),
                        cadence.to_string(),
                        prompt.to_string(),
                        channel,
                        source,
                        next,
                        why,
                    );
                    let store = crate::scheduler::store::SchedulerStore::new(memory.clone());
                    match store.insert(&task) {
                        Ok(_) => format!(
                            "scheduled task '{}' ({}) status={} next_run={}",
                            task.name,
                            task.id,
                            task.status.as_str(),
                            task.next_run_at
                        ),
                        Err(e) => format!("schedule_task insert failed: {}", e),
                    }
                }
                Err(e) => format!("schedule_task: invalid cadence - {}", e),
            }
        }
        "list_schedules" => {
            let store = crate::scheduler::store::SchedulerStore::new(memory.clone());
            match store.list(false) {
                Ok(rows) => serde_json::to_string_pretty(&rows).unwrap_or_default(),
                Err(e) => format!("list_schedules failed: {}", e),
            }
        }
        "cancel_schedule" => {
            let id = s("id");
            let store = crate::scheduler::store::SchedulerStore::new(memory.clone());
            match store.set_status(id, crate::scheduler::TaskStatus::Archived) {
                Ok(_) => format!("cancelled schedule {}", id),
                Err(e) => format!("cancel_schedule failed: {}", e),
            }
        }

        _ => format!("Unknown tool: {}", name),
    }
}

/// Heuristic router: decides if a query is too trivial to justify Team mode.
/// Cheap substring checks so we don't spend a cent just to decide routing.
fn is_trivial_query(message: &str) -> bool {
    let q = message.trim();
    if q.len() < 25 {
        return true;
    }
    // No code-smelling tokens means the user probably just wants to chat.
    let has_code_signal = q.contains('.')
        && (q.contains(".rs")
            || q.contains(".ts")
            || q.contains(".js")
            || q.contains(".py")
            || q.contains(".go")
            || q.contains(".java")
            || q.contains(".cpp")
            || q.contains(".h"));
    let has_shell_signal = q.contains("cargo ")
        || q.contains("npm ")
        || q.contains("git ")
        || q.contains("./")
        || q.contains("sudo ");
    let has_fs_signal = q.contains('/') || q.contains('\\');
    let ql = q.to_ascii_lowercase();
    let has_agentic_verb = ql.contains("build ")
        || ql.contains("run ")
        || ql.contains("install ")
        || ql.contains("fix ")
        || ql.contains("read ")
        || ql.contains("write ")
        || ql.contains("open ")
        || ql.contains("find ")
        || ql.contains("search ")
        || ql.contains("grep ")
        || ql.contains("debug ")
        || ql.contains("refactor ")
        || ql.contains("implement ")
        || ql.contains("create ");

    !(has_code_signal || has_shell_signal || has_fs_signal || has_agentic_verb)
}

fn build_all_tools() -> Vec<crate::llm::types::ToolDefinition> {
    use crate::llm::types::{FunctionDefinition, ToolDefinition};
    let tool = |name: &str, description: &str, params: serde_json::Value| ToolDefinition {
        tool_type: "function".to_string(),
        function: FunctionDefinition {
            name: name.to_string(),
            description: description.to_string(),
            parameters: params,
        },
    };
    vec![
        tool("search_memory", "Search the long-term memory graph for relevant facts, decisions, files, or conversations. Use this BEFORE asking the user to repeat themselves or when you need context from earlier sessions. Returns top-N ranked memory nodes with title, summary, and confidence. Far cheaper than loading the full context packet.", serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type":"string","description":"Natural-language query describing what you need to recall."},
                "limit": {"type":"integer","description":"Max nodes to return. Default 5."}
            },
            "required": ["query"]
        })),

        // -------- computer use (DOM-style) --------
        tool("ui_snapshot", "Snapshot the accessibility tree of the foreground window (or a named one). Returns a JSON tree with stable element ids you can pass to ui_click / ui_type. Call this BEFORE every click so your ids match the current screen. Response also includes your last 8 UI actions so you remember what you already did.", serde_json::json!({
            "type": "object",
            "properties": {
                "window": {"type":"string","description":"Optional substring of a window title (e.g. 'Notepad'). Omit to snapshot the foreground window."},
                "include_offscreen": {"type":"boolean","description":"Include elements scrolled off screen. Default false to keep tokens low."}
            }
        })),
        tool("ui_click", "Click an element by id from the most recent ui_snapshot. Uses UIA's Invoke pattern when available (more reliable than synthetic mouse).", serde_json::json!({
            "type": "object",
            "properties": { "element_id": {"type":"string","description":"ID from the most recent ui_snapshot (e.g. 'e42')."} },
            "required": ["element_id"]
        })),
        tool("ui_type", "Type text into an element by id (typically an edit/value control). Prefers the Value pattern, falls back to keyboard simulation with focus.", serde_json::json!({
            "type": "object",
            "properties": {
                "element_id": {"type":"string"},
                "text": {"type":"string"}
            },
            "required": ["element_id","text"]
        })),
        tool("ui_focus_window", "Bring a window to the foreground by title substring. Snapshot again after focusing to get a fresh tree.", serde_json::json!({
            "type": "object",
            "properties": { "title": {"type":"string","description":"Substring match on window title."} },
            "required": ["title"]
        })),
        tool("ui_find", "Search the most recent snapshot's registry for elements whose name contains the query. Useful when you want to click 'the OK button' without knowing the id.", serde_json::json!({
            "type": "object",
            "properties": { "query": {"type":"string"} },
            "required": ["query"]
        })),

        // -------- scheduler --------
        tool("propose_schedule", "Propose a scheduled task for the user to approve. Use when you think Rook should wake up later and do something (e.g. 'every monday summarize PRs'). Cadence grammar: 'once YYYY-MM-DD HH:MM', 'in 2h', 'daily 09:00', 'weekly mon 09:00', 'every 15m', 'cron 0 9 * * 1'. Status starts as 'proposed' - user must approve before it fires.", serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type":"string","description":"Short human-readable name."},
                "cadence": {"type":"string","description":"Cadence spec - see tool description."},
                "prompt": {"type":"string","description":"What Rook should do when the task fires."},
                "output_channel": {"type":"string","enum":["notification","silent"],"description":"How to deliver the result. Default notification."},
                "why": {"type":"string","description":"Short reason the task is useful. Shown to the user when they approve."}
            },
            "required": ["name","cadence","prompt"]
        })),
        tool("list_schedules", "List all non-archived scheduled tasks. Returns JSON array with id, name, cadence, next_run_at, status.", serde_json::json!({
            "type": "object", "properties": {}
        })),
        tool("cancel_schedule", "Archive a scheduled task by id so it stops firing.", serde_json::json!({
            "type": "object",
            "properties": { "id": {"type":"string"} },
            "required": ["id"]
        })),
        tool("file_read", "Read the contents of a file. Returns content with line numbers in 'NNNN\\tcontent' format so you can reference exact lines in edits. Defaults to first 2000 lines; use offset/limit for windows of larger files.", serde_json::json!({
            "type": "object",
            "properties": {
                "path":   {"type":"string","description":"Absolute or relative file path"},
                "offset": {"type":"integer","description":"0-based line number to start at"},
                "limit":  {"type":"integer","description":"Max lines to return (default 2000)"}
            },
            "required": ["path"]
        })),
        tool("list_files", "List files and directories in a directory. Returns a JSON-like listing with name, type (file|dir), and size. Use this before reading files so you know what exists. Defaults to current working directory.", serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type":"string","description":"Directory to list. Defaults to '.'"},
                "recursive": {"type":"boolean","description":"Walk subdirectories (max depth 3). Default false."},
                "pattern": {"type":"string","description":"Optional glob-like substring filter on entry names"}
            }
        })),
        tool("change_dir", "Change the working directory. Affects subsequent terminal_execute, file_read, file_write, and list_files calls that use relative paths. Returns the new absolute cwd.", serde_json::json!({
            "type": "object",
            "properties": { "path": {"type":"string","description":"Target directory (absolute or relative)"} },
            "required": ["path"]
        })),
        tool("get_cwd", "Return the current working directory as an absolute path.", serde_json::json!({ "type": "object", "properties": {} })),
        tool("glob", "Find files by glob pattern (e.g. 'src/**/*.rs', '**/*.toml'). Returns matching file paths sorted by modification time. Faster than list_files for finding specific file types in large trees.", serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {"type":"string","description":"Glob pattern, e.g. 'src/**/*.ts'"},
                "path": {"type":"string","description":"Optional base directory (default: cwd)"}
            },
            "required": ["pattern"]
        })),
        tool("grep", "Search file contents using ripgrep semantics. Use this BEFORE file_read to locate the relevant lines. Returns matching files (default) or matching lines with file:line:content.", serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {"type":"string","description":"Regex pattern"},
                "path": {"type":"string","description":"Directory or file to search (default: cwd)"},
                "glob": {"type":"string","description":"Optional glob filter, e.g. '*.rs'"},
                "output": {"type":"string","enum":["files","content","count"],"description":"files=just paths (default), content=matching lines, count=match counts per file"},
                "case_insensitive": {"type":"boolean"},
                "max_results": {"type":"integer","description":"Cap results (default 200)"}
            },
            "required": ["pattern"]
        })),
        tool("todo_write", "Create or update the task list. Use ONLY for complex tasks with 3+ distinct steps - NOT for simple or single-step work. Pass the COMPLETE list each time (replaces previous). Each item: { content (imperative), activeForm (present continuous), status (pending|in_progress|completed) }. Keep exactly ONE task in_progress. Mark completed IMMEDIATELY after finishing.", serde_json::json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type":"string","description":"Imperative form, e.g. 'Run tests'"},
                            "activeForm": {"type":"string","description":"Present continuous, e.g. 'Running tests'"},
                            "status": {"type":"string","enum":["pending","in_progress","completed"]}
                        },
                        "required": ["content","activeForm","status"]
                    }
                }
            },
            "required": ["todos"]
        })),
        tool("file_write", "Write content to a file, creating it if it doesn't exist.", serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type":"string"},
                "content": {"type":"string"}
            },
            "required": ["path","content"]
        })),
        tool("code_edit", "Edit a file. Actions: search_replace, insert_at_line, append.", serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type":"string","enum":["search_replace","insert_at_line","append"]},
                "path": {"type":"string"},
                "line": {"type":"integer","description":"Line number for insert_at_line"},
                "content": {"type":"string","description":"Content for append or insert_at_line"},
                "search": {"type":"string","description":"Text to find for search_replace"},
                "replace": {"type":"string","description":"Replacement text for search_replace"}
            },
            "required": ["action","path"]
        })),
        tool("terminal_execute", "Run a shell command and return stdout+stderr. Use for builds, tests, installs, etc.", serde_json::json!({
            "type": "object",
            "properties": { "command": {"type":"string"} },
            "required": ["command"]
        })),
        tool("git_status", "Show git status of the workspace.", serde_json::json!({ "type": "object", "properties": {} })),
        tool("git_diff", "Show git diff. Optionally for a specific file.", serde_json::json!({
            "type": "object",
            "properties": { "file": {"type":"string","description":"Optional file path"} }
        })),
        tool("git_log", "Show recent git commits.", serde_json::json!({
            "type": "object",
            "properties": { "count": {"type":"integer","description":"Number of commits (default 10)"} }
        })),
        tool("git_branch", "List git branches and show current branch.", serde_json::json!({ "type": "object", "properties": {} })),
        tool("git_commit", "Stage files and create a git commit. Only commit when the user asks. Provide a concise message describing the change.", serde_json::json!({
            "type": "object",
            "properties": {
                "message": {"type":"string","description":"Commit message"},
                "files": {"type":"array","items":{"type":"string"},"description":"Files to stage (relative or absolute paths). Use [\".\"] to stage all changes."}
            },
            "required": ["message","files"]
        })),
        tool("web_search", "Search the web using DuckDuckGo. Returns titles, URLs, and snippets.", serde_json::json!({
            "type": "object",
            "properties": { "query": {"type":"string"} },
            "required": ["query"]
        })),
        tool("fetch_url", "Fetch and extract text content from a URL.", serde_json::json!({
            "type": "object",
            "properties": { "url": {"type":"string"} },
            "required": ["url"]
        })),
        tool("browser_navigate", "Open a URL in the headless browser and return page text.", serde_json::json!({
            "type": "object",
            "properties": { "url": {"type":"string"} },
            "required": ["url"]
        })),
        tool("browser_click", "Click a CSS selector in the browser.", serde_json::json!({
            "type": "object",
            "properties": { "selector": {"type":"string"} },
            "required": ["selector"]
        })),
        tool("browser_type", "Type text into a form element matched by CSS selector.", serde_json::json!({
            "type": "object",
            "properties": {
                "selector": {"type":"string"},
                "text": {"type":"string"}
            },
            "required": ["selector","text"]
        })),
        tool("browser_evaluate", "Evaluate JavaScript in the browser and return the result.", serde_json::json!({
            "type": "object",
            "properties": { "js": {"type":"string"} },
            "required": ["js"]
        })),
        tool("browser_screenshot", "Take a screenshot of the current browser page. Returns base64 PNG.", serde_json::json!({
            "type": "object",
            "properties": { "full_page": {"type":"boolean","description":"Capture full scrollable page"} }
        })),
        tool("store_memory", "Store a piece of information in long-term memory for future retrieval.", serde_json::json!({
            "type": "object",
            "properties": {
                "content": {"type":"string","description":"The information to remember"},
                "category": {"type":"string","description":"Category tag (e.g. 'project', 'decision', 'architecture', 'bug', 'preference')"}
            },
            "required": ["content"]
        })),
        tool("store_user_fact", "Store or update a fact about the user in the global profile.", serde_json::json!({
            "type": "object",
            "properties": {
                "key": {"type":"string","description":"Fact key (e.g. 'name', 'preferred_language', 'editor', 'timezone')"},
                "value": {"type":"string","description":"Fact value"}
            },
            "required": ["key","value"]
        })),
        tool("shell_spawn", "Create a new named persistent shell (e.g. 'shell2', 'build', 'server'). The shell keeps its own cwd and env across multiple shell_exec calls. If a shell with the same name already exists, returns an error - kill it first with shell_kill. Use this when you need a SECOND shell separate from the default (e.g. running a dev server in the background while you work in another shell).", serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type":"string","description":"Shell name, e.g. 'shell2'"}
            },
            "required": ["name"]
        })),
        tool("shell_exec", "Run a command in a specific named shell. cwd persists across calls. Set run_in_background=true for commands you want to keep running while you do other work (dev servers, log watchers, etc) - the tool returns immediately and you fetch output later with shell_read. Set timeout_secs for synchronous calls (default 45s).", serde_json::json!({
            "type": "object",
            "properties": {
                "shell": {"type":"string","description":"Shell name (default: 'default'). Auto-creates if missing."},
                "command": {"type":"string","description":"Shell command to execute"},
                "run_in_background": {"type":"boolean","description":"Start command in background and return immediately"},
                "timeout_secs": {"type":"integer","description":"Override default 45s timeout for sync calls"}
            },
            "required": ["command"]
        })),
        tool("shell_read", "Read accumulated output from a shell's background process + buffer. Use after starting a background command with shell_exec to check its progress.", serde_json::json!({
            "type": "object",
            "properties": {
                "shell": {"type":"string","description":"Shell name (default: 'default')"},
                "clear": {"type":"boolean","description":"Clear the buffer after reading"}
            }
        })),
        tool("shell_kill", "Terminate a named shell and its background process. Use when you're done with a dev server or want to reclaim a shell name.", serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type":"string","description":"Shell name to kill"}
            },
            "required": ["name"]
        })),
        tool("shell_list", "List all active shells with their cwd, background state, and buffer size.", serde_json::json!({
            "type": "object",
            "properties": {}
        })),
        tool("outline_file", "Return a symbolic outline of a source file - function names, class names, imports - with their line numbers. Much cheaper than file_read when you only need to understand a file's structure for navigation. Supports Rust, JS, TS, Python, Go.", serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type":"string","description":"Path to source file"}
            },
            "required": ["path"]
        })),
        tool("search_in_file", "Search for a regex pattern within a single file and return matching lines with surrounding context. Use this instead of grep when you already know which file to search.", serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type":"string","description":"File to search"},
                "pattern": {"type":"string","description":"Regex pattern"},
                "context": {"type":"integer","description":"Lines of context around each match (default 2)"},
                "case_insensitive": {"type":"boolean"}
            },
            "required": ["path","pattern"]
        })),
        tool("file_diff", "Show a unified diff of what's changed in a file since the first read/edit in THIS conversation. Use this to verify what your edits actually did.", serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type":"string","description":"File to diff"}
            },
            "required": ["path"]
        })),
        tool("file_undo", "Restore a file to its state from the start of this conversation (from the snapshot taken on first read/edit). Use when an edit went wrong.", serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type":"string","description":"File to restore"}
            },
            "required": ["path"]
        })),
        tool("lsp_definition", "Go to the definition of the symbol at a given line and column. Returns the file path and line number of the definition. Requires a language server (rust-analyzer, typescript-language-server, pylsp) on PATH.", serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type":"string","description":"Source file path"},
                "line": {"type":"integer","description":"0-based line number"},
                "character": {"type":"integer","description":"0-based column number"}
            },
            "required": ["path","line","character"]
        })),
        tool("lsp_references", "Find all references to the symbol at a given position. Returns file:line:col for each reference.", serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type":"string","description":"Source file path"},
                "line": {"type":"integer","description":"0-based line number"},
                "character": {"type":"integer","description":"0-based column number"}
            },
            "required": ["path","line","character"]
        })),
        tool("lsp_hover", "Get type information and documentation for the symbol at a given position.", serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type":"string","description":"Source file path"},
                "line": {"type":"integer","description":"0-based line number"},
                "character": {"type":"integer","description":"0-based column number"}
            },
            "required": ["path","line","character"]
        })),
        tool("apply_patch", "Apply a unified diff patch to a file. Pass the file path and the patch text in standard unified diff format (@@-hunk style). Snapshots the file first so file_undo still works. Use this instead of code_edit when you have a pre-computed diff.", serde_json::json!({
            "type": "object",
            "properties": {
                "path":  {"type":"string","description":"File to patch"},
                "patch": {"type":"string","description":"Unified diff text (@@-hunk format, optionally with --- +++ header)"}
            },
            "required": ["path","patch"]
        })),
    ]
}

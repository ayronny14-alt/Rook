use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::llm::client::LLMClient;
use crate::memory::context::ContextCurator;
use crate::memory::graph::GraphMemory;
use crate::memory::object::ObjectMemoryStore;

use rusqlite::params;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextPacket {
    pub active: ActiveWindow,
    pub task: TaskWindow,
    pub memory: Vec<crate::memory::context::RankedContextNode>,
    pub user: String,
}
use crate::memory::storage::MemoryStorage;

const DEFAULT_MAX_PACKET_CHARS: usize = 6_000;
const DEFAULT_ACTIVE_KEEP: usize = 4;
const DEFAULT_MEMORY_TOPK: usize = 6;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveWindow {
    pub recent_user: Vec<String>,
    pub recent_assistant: Vec<String>,
    pub last_tool_result: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskWindow {
    pub current_task: Option<String>,
    pub active_files: Vec<String>,
    pub active_symbols: Vec<String>,
    pub active_graph_nodes: Vec<String>,
    pub last_tool_calls: Vec<String>,
    pub last_retrieved_chunks: Vec<String>,
}

pub struct SlidingWindow {
    storage: MemoryStorage,
}

impl SlidingWindow {
    pub fn new(storage: MemoryStorage) -> Self {
        Self { storage }
    }

    pub async fn assemble_context_packet(
        &self,
        llm: &LLMClient,
        user_message: &str,
        conversation_id: Option<&str>,
        max_chars: Option<usize>,
    ) -> Result<(String, serde_json::Value)> {
        let max_chars = max_chars.unwrap_or(DEFAULT_MAX_PACKET_CHARS);

        // Active window: include the last 2 assistant replies from the conversation so
        // the model has cross-turn continuity without needing the full message history.
        let recent_assistant = if let Some(conv) = conversation_id {
            get_recent_messages(&self.storage, conv, 6)
                .unwrap_or_default()
                .into_iter()
                .filter(|(role, _, _)| role == "assistant")
                .map(|(_, content, _)| {
                    let truncated: String = content.chars().take(600).collect();
                    truncated
                })
                .take(2)
                .collect()
        } else {
            vec![]
        };

        let active = ActiveWindow {
            recent_user: vec![user_message.to_string()],
            recent_assistant,
            last_tool_result: None,
        };

        // Task window: surface a few task nodes (titles) from the graph if available.
        let task = self.assemble_task_window()?;

        // Memory window: use existing ContextCurator to get ranked nodes.
        let curator = ContextCurator::new(self.storage.clone());
        let ranked = curator
            .curate_for_query(
                llm,
                user_message,
                Some(DEFAULT_MEMORY_TOPK * 8),
                Some(DEFAULT_MEMORY_TOPK),
            )
            .await
            .unwrap_or_default();

        // Query-type routing: classify the query to bias retrieval
        let qtype = classify_query(user_message);

        // Recency boosting: collect recent node ids used in this conversation
        let recent_node_ids = if let Some(conv) = conversation_id {
            collect_recent_node_ids(&self.storage, conv, 4).unwrap_or_default()
        } else {
            Vec::new()
        };

        // Re-score / bias ranked nodes according to query type and recency
        let mut boosted: Vec<crate::memory::context::RankedContextNode> = ranked
            .clone()
            .into_iter()
            .map(|mut r| {
                let type_boost = match qtype {
                    QueryType::Code => {
                        if r.node_type.to_lowercase().contains("file")
                            || r.node_type.to_lowercase().contains("symbol")
                            || r.node_type.to_lowercase().contains("chunk")
                        {
                            0.12
                        } else {
                            0.0
                        }
                    }
                    QueryType::Ui => {
                        if r.node_type.to_lowercase().contains("task")
                            || r.node_type.to_lowercase().contains("tool")
                        {
                            0.10
                        } else {
                            0.0
                        }
                    }
                    QueryType::Troubleshoot => {
                        if r.node_type.to_lowercase().contains("error")
                            || r.node_type.to_lowercase().contains("log")
                        {
                            0.15
                        } else {
                            0.0
                        }
                    }
                    QueryType::Conceptual => 0.0,
                    QueryType::Other => 0.0,
                };

                let recency_boost = if recent_node_ids.iter().any(|id| id == &r.node_id) {
                    0.08
                } else {
                    0.0
                };

                r.total_score = (r.total_score + type_boost + recency_boost).clamp(0.0, 10.0);
                r
            })
            .collect();

        // Sort and take top-K
        boosted.sort_by(|a, b| {
            b.total_score
                .partial_cmp(&a.total_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        boosted.truncate(DEFAULT_MEMORY_TOPK);

        // Compress memory blocks
        let compressed = compress_ranked_nodes(&boosted);

        // Infer task from top-ranked memory node and pull active files/symbols
        // from recent conversation messages.
        let mut task = task;
        if task.current_task.is_none() {
            task.current_task = compressed.first().map(|n| n.title.clone());
        }
        if let Some(conv) = conversation_id {
            if let Ok(msgs) = get_recent_messages(&self.storage, conv, 5) {
                let (files, symbols) = extract_files_and_symbols_from_messages(&msgs);
                task.active_files = files;
                task.active_symbols = symbols;
            }
        }

        // Working set: top memory node titles as active graph nodes
        task.active_graph_nodes = compressed.iter().map(|r| r.title.clone()).take(6).collect();
        task.last_retrieved_chunks = compressed
            .iter()
            .map(|r| r.summary.clone().unwrap_or_default())
            .take(6)
            .collect();

        // Format sections
        let mut parts: Vec<String> = Vec::new();

        parts.push("ACTIVE CONTEXT:".to_string());
        for u in active.recent_user.iter().take(DEFAULT_ACTIVE_KEEP) {
            parts.push(format!("- USER: {}", truncate_for(u, 400)));
        }
        for a in active.recent_assistant.iter().take(DEFAULT_ACTIVE_KEEP) {
            parts.push(format!("- ASSISTANT: {}", truncate_for(a, 400)));
        }
        if let Some(tool) = &active.last_tool_result {
            parts.push(format!("- LAST TOOL RESULT: {}", truncate_for(tool, 800)));
        }

        parts.push("\nTASK CONTEXT:".to_string());
        if let Some(t) = &task.current_task {
            parts.push(format!("- CURRENT TASK: {}", truncate_for(t, 400)));
        }
        if !task.active_graph_nodes.is_empty() {
            parts.push(format!(
                "- ACTIVE NODES: {}",
                task.active_graph_nodes.join(", ")
            ));
        }

        parts.push("\nMEMORY CONTEXT:".to_string());

        // Build structured packet and persist
        let packet_struct = ContextPacket {
            active: active.clone(),
            task: task.clone(),
            memory: compressed.clone(),
            user: user_message.to_string(),
        };

        let mut mem_lines = crate::memory::context::ContextCurator::format_context_packet(&boosted);

        let mut candidate_packet = format!(
            "{}\n{}\nUSER:\n{}",
            parts.join("\n"),
            mem_lines,
            user_message
        );

        // If too big, try reducing memory top-k by trimming printed memory lines
        if candidate_packet.chars().count() > max_chars {
            // progressively reduce memory nodes until fit
            let mut reduced_k = ranked.len().saturating_sub(1);
            while reduced_k > 0 {
                let truncated = ranked.iter().take(reduced_k).cloned().collect::<Vec<_>>();
                mem_lines =
                    crate::memory::context::ContextCurator::format_context_packet(&truncated);
                candidate_packet = format!(
                    "{}\n{}\nUSER:\n{}",
                    parts.join("\n"),
                    mem_lines,
                    user_message
                );
                if candidate_packet.chars().count() <= max_chars {
                    break;
                }
                reduced_k = reduced_k.saturating_sub(1);
            }
        }

        // Final safety truncate
        if candidate_packet.chars().count() > max_chars {
            candidate_packet = candidate_packet
                .chars()
                .take(max_chars.saturating_sub(50))
                .collect();
            candidate_packet.push_str("\n... [truncated context due to size] ...");
        }

        // persist packet and return structured value
        let packet_json = serde_json::to_value(&packet_struct)?;
        let _ = self.persist_packet(conversation_id, &packet_struct);

        Ok((candidate_packet, packet_json))
    }

    fn persist_packet(&self, conversation_id: Option<&str>, packet: &ContextPacket) -> Result<()> {
        let conn = self
            .storage
            .get_connection()
            .map_err(|e| anyhow::anyhow!(e))?;
        let id = Uuid::new_v4().to_string();
        let packet_json = serde_json::to_string(packet)?;
        let conv = conversation_id.unwrap_or("");
        conn.execute(
            "INSERT INTO context_packets (id, conversation_id, packet_json) VALUES (?1, ?2, ?3)",
            rusqlite::params![id, conv, packet_json],
        )?;

        // Upsert task state if present
        if let Some(task_text) = packet.task.current_task.as_ref() {
            let working_set = serde_json::to_string(&packet.task)?;
            conn.execute(
                "INSERT OR REPLACE INTO tasks (conversation_id, task_text, working_set_json, updated_at) VALUES (?1, ?2, ?3, strftime('%s', 'now'))",
                rusqlite::params![conv, task_text, working_set],
            )?;
        }

        Ok(())
    }

    fn assemble_task_window(&self) -> Result<TaskWindow> {
        let graph = GraphMemory::new(self.storage.clone());
        let _object_store = ObjectMemoryStore::new(self.storage.clone());

        // Collect recent Task nodes titles to surface as current task hints
        let mut active_nodes = Vec::new();
        for n in graph.search_nodes(Some(crate::memory::graph::NodeType::Task), None)? {
            active_nodes.push(n.title);
            if active_nodes.len() >= 6 {
                break;
            }
        }

        Ok(TaskWindow {
            current_task: active_nodes.first().cloned(),
            active_files: Vec::new(),
            active_symbols: Vec::new(),
            active_graph_nodes: active_nodes,
            last_tool_calls: Vec::new(),
            last_retrieved_chunks: Vec::new(),
        })
    }
}

fn truncate_for(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max_chars).collect();
    format!("{}…", truncated)
}

fn classify_query(q: &str) -> QueryType {
    let ql = q.to_ascii_lowercase();
    // Check Code before Ui — "how do I build this?" should be Code, not Ui.
    // Check Troubleshoot before Ui — "error when I run" should be Troubleshoot.
    if ql.contains("error")
        || ql.contains("stack trace")
        || ql.contains("panic")
        || ql.contains("crash")
        || ql.contains("fail")
        || ql.contains("exception")
        || ql.contains("warning:")
        || ql.contains("undefined")
        || ql.contains("cannot find")
    {
        return QueryType::Troubleshoot;
    }
    if ql.contains("fn ")
        || ql.contains("function")
        || ql.contains("struct ")
        || ql.contains("impl ")
        || ql.contains("trait ")
        || ql.contains("mod ")
        || ql.contains("symbol")
        || ql.contains("compile")
        || ql.contains("build")
        || ql.contains("cargo")
        || ql.contains("syntax")
        || ql.contains("code")
        || ql.contains("refactor")
        || ql.contains("implement")
    {
        return QueryType::Code;
    }
    if ql.contains("what is")
        || ql.contains("explain")
        || ql.contains("why")
        || ql.contains("how does")
        || ql.contains("what does")
        || ql.contains("describe")
        || ql.contains("difference between")
    {
        return QueryType::Conceptual;
    }
    if ql.contains("how do i")
        || ql.contains("start")
        || ql.contains("run")
        || ql.contains("launch")
        || ql.contains("open")
        || ql.contains("click")
        || ql.contains(" ui")
        || ql.contains("button")
        || ql.contains("window")
        || ql.contains("install")
        || ql.contains("setup")
        || ql.contains("configure")
    {
        return QueryType::Ui;
    }
    QueryType::Other
}

#[derive(Debug, Clone, Copy)]
enum QueryType {
    Code,
    Ui,
    Troubleshoot,
    Conceptual,
    Other,
}

fn compress_ranked_nodes(
    nodes: &[crate::memory::context::RankedContextNode],
) -> Vec<crate::memory::context::RankedContextNode> {
    let mut out = Vec::new();
    for n in nodes.iter() {
        if n.confidence_score < 0.15 {
            continue;
        }
        let mut n2 = n.clone();
        if let Some(s) = &n2.summary {
            n2.summary = Some(truncate_for(s, 140));
        }
        if n2.key_facts.len() > 3 {
            n2.key_facts.truncate(3);
        }
        out.push(n2);
    }
    out
}

fn get_recent_messages(
    storage: &MemoryStorage,
    conversation_id: &str,
    limit: usize,
) -> Result<Vec<(String, String, Option<String>)>> {
    let mut out = Vec::new();
    let conn = storage.get_connection().map_err(|e| anyhow::anyhow!(e))?;
    let mut stmt = conn.prepare("SELECT role, content, tool_calls_json FROM messages WHERE conversation_id = ?1 ORDER BY created_at DESC LIMIT ?2")?;
    let mut rows = stmt.query(params![conversation_id, limit])?;
    while let Some(r) = rows.next()? {
        let role: String = r.get(0)?;
        let content: String = r.get(1)?;
        let tool_calls: Option<String> = r.get(2)?;
        out.push((role, content, tool_calls));
    }
    Ok(out)
}

fn extract_files_and_symbols_from_messages(
    msgs: &[(String, String, Option<String>)],
) -> (Vec<String>, Vec<String>) {
    const FILE_EXTS: [&str; 5] = [".rs", ".cpp", ".ts", ".js", ".py"];
    let mut files = Vec::new();
    let mut syms = Vec::new();
    for (_role, content, _calls) in msgs.iter() {
        for token in content.split_whitespace() {
            if FILE_EXTS.iter().any(|ext| token.ends_with(ext)) {
                files.push(
                    token
                        .trim_matches(|c: char| c.is_ascii_punctuation())
                        .to_string(),
                );
            }
            if token.contains("::") || token.contains("(") {
                syms.push(
                    token
                        .trim_matches(|c: char| c.is_ascii_punctuation())
                        .to_string(),
                );
            }
        }
    }
    files.dedup();
    syms.dedup();
    (files, syms)
}

fn collect_recent_node_ids(
    storage: &MemoryStorage,
    conversation_id: &str,
    limit: usize,
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let conn = storage.get_connection().map_err(|e| anyhow::anyhow!(e))?;
    let mut stmt = conn.prepare("SELECT packet_json FROM context_packets WHERE conversation_id = ?1 ORDER BY created_at DESC LIMIT ?2")?;
    let mut rows = stmt.query(params![conversation_id, limit])?;
    while let Some(r) = rows.next()? {
        let json_text: String = r.get(0)?;
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&json_text) {
            if let Some(mem) = v.get("memory").and_then(|m| m.as_array()) {
                for item in mem.iter() {
                    if let Some(id) = item.get("node_id").and_then(|i| i.as_str()) {
                        out.push(id.to_string());
                    }
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::client::LLMClient;
    use crate::llm::types::{EmbeddingProvider, LLMConfig};

    fn make_mock_llm() -> LLMClient {
        LLMClient::new(LLMConfig {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: String::new(),
            model: "gpt-4o-mini".to_string(),
            max_tokens: 512,
            temperature: 0.2,
            embedding_provider: EmbeddingProvider::Mock,
            embedding_base_url: "http://127.0.0.1:11434".to_string(),
            embedding_model: "nomic-embed-text".to_string(),
            embedding_api_key: String::new(),
        })
    }

    #[tokio::test]
    async fn assemble_packet_is_compact_and_contains_sections() {
        let storage = crate::memory::storage::MemoryStorage::new_in_memory().unwrap();
        let llm = make_mock_llm();
        let sliding = SlidingWindow::new(storage.clone());

        let (packet, _packet_json) = sliding
            .assemble_context_packet(&llm, "How to start the UI?", None, Some(2000))
            .await
            .unwrap();

        assert!(packet.contains("ACTIVE CONTEXT:"));
        assert!(packet.contains("TASK CONTEXT:"));
        assert!(packet.contains("MEMORY CONTEXT:"));
        assert!(packet.contains("USER:"));
        assert!(packet.chars().count() <= 2000);

        // ensure persisted
        let conn = storage.get_connection().unwrap();
        let mut stmt = conn
            .prepare("SELECT COUNT(*) FROM context_packets")
            .unwrap();
        let count: i64 = stmt.query_row([], |r| r.get(0)).unwrap();
        assert!(count >= 1);
    }
}

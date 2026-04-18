use anyhow::Result;
use tracing::warn;

use super::HandlerCtx;
use crate::ipc::protocol::IPCResponse;

pub async fn handle_get_conversations(ctx: &HandlerCtx, id: &str) -> Result<IPCResponse> {
    let rows: Vec<serde_json::Value> = match ctx.memory.get_connection() {
        Ok(conn) => {
            // Ensure pinned column exists (idempotent migration)
            let _ = conn.execute(
                "ALTER TABLE conversations ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0",
                [],
            );
            conn
                .prepare("SELECT id, title, created_at, updated_at, COALESCE(pinned, 0) as pinned FROM conversations ORDER BY pinned DESC, updated_at DESC LIMIT 200")
                .and_then(|mut stmt| {
                    stmt.query_map([], |row| {
                        Ok(serde_json::json!({
                            "id":         row.get::<_, String>(0)?,
                            "title":      row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                            "created_at": row.get::<_, i64>(2)?,
                            "updated_at": row.get::<_, i64>(3)?,
                            "pinned":     row.get::<_, i64>(4)? != 0,
                        }))
                    })
                    .and_then(|r| r.collect::<rusqlite::Result<Vec<_>>>())
                })
                .unwrap_or_default()
        }
        Err(_) => vec![],
    };
    Ok(IPCResponse::ConversationList {
        id: id.to_string(),
        conversations: rows,
    })
}

pub async fn handle_search_conversations(
    ctx: &HandlerCtx,
    id: &str,
    query: &str,
) -> Result<IPCResponse> {
    let pattern = format!("%{}%", query);
    let rows: Vec<serde_json::Value> = match ctx.memory.get_connection() {
        Ok(conn) => conn
            .prepare(
                "SELECT DISTINCT c.id, c.title, c.updated_at
                 FROM conversations c
                 LEFT JOIN messages m ON m.conversation_id = c.id
                 WHERE c.title LIKE ?1 OR m.content LIKE ?1
                 ORDER BY c.updated_at DESC LIMIT 50",
            )
            .and_then(|mut stmt| {
                stmt.query_map(rusqlite::params![&pattern], |row| {
                    Ok(serde_json::json!({
                        "id":         row.get::<_, String>(0)?,
                        "title":      row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        "updated_at": row.get::<_, i64>(2)?,
                    }))
                })
                .and_then(|r| r.collect::<rusqlite::Result<Vec<_>>>())
            })
            .unwrap_or_default(),
        Err(_) => vec![],
    };
    Ok(IPCResponse::ConversationList {
        id: id.to_string(),
        conversations: rows,
    })
}

pub async fn handle_get_conversation_messages(
    ctx: &HandlerCtx,
    id: &str,
    conversation_id: &str,
) -> Result<IPCResponse> {
    let msgs: Vec<serde_json::Value> = match ctx.memory.get_connection() {
        Ok(conn) => conn
            .prepare(
                "SELECT id, role, content, created_at FROM messages \
                 WHERE conversation_id = ?1 ORDER BY created_at ASC",
            )
            .and_then(|mut stmt| {
                stmt.query_map(rusqlite::params![conversation_id], |row| {
                    Ok(serde_json::json!({
                        "id":         row.get::<_, String>(0)?,
                        "role":       row.get::<_, String>(1)?,
                        "content":    row.get::<_, String>(2)?,
                        "created_at": row.get::<_, i64>(3)?,
                    }))
                })
                .and_then(|r| r.collect::<rusqlite::Result<Vec<_>>>())
            })
            .unwrap_or_default(),
        Err(_) => vec![],
    };
    Ok(IPCResponse::ConversationMessages {
        id: id.to_string(),
        conversation_id: conversation_id.to_string(),
        messages: msgs,
    })
}

/// After the 2nd user message, fire a cheap title-generation call.
/// Takes only what it needs (memory + llm) so chat.rs can spawn it without cloning HandlerCtx.
pub async fn auto_title_if_ready(
    memory: &crate::memory::storage::MemoryStorage,
    llm: &crate::llm::client::LLMClient,
    conversation_id: &str,
) -> Option<String> {
    // Count user messages
    let user_count: i64 = memory
        .get_connection()
        .ok()
        .and_then(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1 AND role = 'user'",
                rusqlite::params![conversation_id],
                |r| r.get(0),
            )
            .ok()
        })
        .unwrap_or(0);

    if user_count < 2 {
        return None;
    }

    // Skip if already has a meaningful title (not just the raw first message truncated)
    let existing_title: Option<String> = memory.get_connection().ok().and_then(|conn| {
        conn.query_row(
            "SELECT title FROM conversations WHERE id = ?1",
            rusqlite::params![conversation_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
    });
    if let Some(ref t) = existing_title {
        // A "real" title has been set already — skip
        if t.chars().count() < 60 && !t.is_empty() && user_count > 2 {
            return None;
        }
    }

    // Grab the first four messages for context
    let context: Vec<(String, String)> = memory.get_connection().ok().and_then(|conn| {
        conn.prepare(
            "SELECT role, content FROM messages WHERE conversation_id = ?1 ORDER BY created_at ASC LIMIT 4",
        )
        .and_then(|mut s| {
            s.query_map(rusqlite::params![conversation_id], |r| {
                Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?))
            })
            .and_then(|r| r.collect::<rusqlite::Result<Vec<_>>>())
        }).ok()
    }).unwrap_or_default();

    if context.is_empty() {
        return None;
    }

    let snippet: String = context
        .iter()
        .map(|(r, c)| format!("{}: {}", r, c.chars().take(120).collect::<String>()))
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = format!(
        "Summarise this conversation in ≤6 words (title case, no punctuation):\n\n{}\n\nTitle:",
        snippet
    );

    let messages = vec![crate::llm::types::Message::text("user", prompt)];
    let title = match llm.chat(messages).await {
        Ok(resp) => {
            let raw = resp
                .choices
                .into_iter()
                .next()
                .and_then(|c| c.message.text_content())
                .unwrap_or_default();
            // Model may echo the prompt or include "Title: <answer>" — extract just the answer.
            // 1. If "Title:" appears, take everything after the last occurrence.
            // 2. Take only the first non-empty line (strips any prompt echo above).
            // 3. Strip quotes and whitespace, cap at 60 chars.
            let after_marker = if let Some(pos) = raw.rfind("Title:") {
                raw[pos + 6..].to_string()
            } else {
                raw.clone()
            };
            let first_line = after_marker
                .lines()
                .map(|l| l.trim().trim_matches('"').trim())
                .find(|l| !l.is_empty())
                .unwrap_or("")
                .to_string();
            first_line.chars().take(60).collect::<String>()
        }
        Err(e) => {
            warn!("Auto-title failed: {}", e);
            context
                .iter()
                .find(|(r, _)| r == "user")
                .map(|(_, c)| c.chars().take(40).collect())
                .unwrap_or_else(|| "Conversation".to_string())
        }
    };

    if title.is_empty() {
        return None;
    }

    // Persist
    if let Ok(conn) = memory.get_connection() {
        let _ = conn.execute(
            "UPDATE conversations SET title = ?1, updated_at = strftime('%s','now') WHERE id = ?2",
            rusqlite::params![&title, conversation_id],
        );
    }

    Some(title)
}

/// Wrapper that takes HandlerCtx (kept for backward compatibility, calls the free fn).
pub async fn maybe_auto_title(ctx: &HandlerCtx, conversation_id: &str) -> Option<String> {
    auto_title_if_ready(&ctx.memory, &ctx.llm, conversation_id).await
}

pub async fn handle_regenerate_last_message(
    ctx: &HandlerCtx,
    id: &str,
    conversation_id: &str,
    model: Option<&str>,
) -> Result<IPCResponse> {
    // Delete the last assistant message
    if let Ok(conn) = ctx.memory.get_connection() {
        let _ = conn.execute(
            "DELETE FROM messages WHERE id = (
                SELECT id FROM messages
                WHERE conversation_id = ?1 AND role = 'assistant'
                ORDER BY created_at DESC LIMIT 1
            )",
            rusqlite::params![conversation_id],
        );
    }

    // Get the last user message content
    let last_user_msg: Option<String> = ctx.memory.get_connection().ok().and_then(|conn| {
        conn.query_row(
            "SELECT content FROM messages WHERE conversation_id = ?1 AND role = 'user' ORDER BY created_at DESC LIMIT 1",
            rusqlite::params![conversation_id],
            |r| r.get(0),
        ).ok()
    });

    match last_user_msg {
        Some(msg) => {
            // Re-run the chat handler
            crate::ipc::handlers::chat::handle_chat(
                ctx,
                id,
                Some(conversation_id),
                &msg,
                None,
                model,
                None,
                None,
                None,
            )
            .await
        }
        None => Ok(IPCResponse::Error {
            id: id.to_string(),
            message: "No user message found to regenerate from".to_string(),
        }),
    }
}

pub async fn handle_delete_conversation(
    ctx: &HandlerCtx,
    id: &str,
    conversation_id: &str,
) -> Result<IPCResponse> {
    if let Ok(conn) = ctx.memory.get_connection() {
        let _ = conn.execute(
            "DELETE FROM messages WHERE conversation_id = ?1",
            rusqlite::params![conversation_id],
        );
        let _ = conn.execute(
            "DELETE FROM conversations WHERE id = ?1",
            rusqlite::params![conversation_id],
        );
    }
    // Clean up in-memory session state (todos, cwd, file snapshots)
    super::chat::clear_conversation_state(conversation_id);
    Ok(IPCResponse::ConversationDeleted {
        id: id.to_string(),
        conversation_id: conversation_id.to_string(),
    })
}

pub async fn handle_rename_conversation(
    ctx: &HandlerCtx,
    id: &str,
    conversation_id: &str,
    title: &str,
) -> Result<IPCResponse> {
    if let Ok(conn) = ctx.memory.get_connection() {
        let _ = conn.execute(
            "UPDATE conversations SET title = ?1, updated_at = strftime('%s','now') WHERE id = ?2",
            rusqlite::params![title, conversation_id],
        );
    }
    Ok(IPCResponse::ConversationRenamed {
        id: id.to_string(),
        conversation_id: conversation_id.to_string(),
        title: title.to_string(),
    })
}

pub async fn handle_pin_conversation(
    ctx: &HandlerCtx,
    id: &str,
    conversation_id: &str,
    pinned: bool,
) -> Result<IPCResponse> {
    if let Ok(conn) = ctx.memory.get_connection() {
        // Ensure column exists
        let _ = conn.execute(
            "ALTER TABLE conversations ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute(
            "UPDATE conversations SET pinned = ?1 WHERE id = ?2",
            rusqlite::params![pinned as i32, conversation_id],
        );
    }
    Ok(IPCResponse::ConversationPinned {
        id: id.to_string(),
        conversation_id: conversation_id.to_string(),
        pinned,
    })
}

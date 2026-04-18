use anyhow::Result;
use tracing::info;

use super::HandlerCtx;
use crate::ipc::protocol::IPCResponse;

pub async fn handle_graceful_shutdown(_ctx: &HandlerCtx, id: &str) -> Result<IPCResponse> {
    info!("Graceful shutdown requested via IPC — flushing WAL");
    // WAL checkpoint + flush. Best-effort; if it fails we allow extra drain time.
    let wal_ok = _ctx
        .memory
        .get_connection()
        .ok()
        .map(|conn| {
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
                .is_ok()
        })
        .unwrap_or(false);
    if wal_ok {
        info!("WAL checkpoint succeeded");
    } else {
        info!("WAL checkpoint failed or skipped — allowing extra drain time");
    }
    // Schedule process exit after returning the response. Give more time if flush failed.
    tokio::spawn(async move {
        let delay = if wal_ok { 300 } else { 1500 };
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        std::process::exit(0);
    });
    Ok(IPCResponse::Ok { id: id.to_string() })
}

pub async fn handle_get_user_fact(ctx: &HandlerCtx, id: &str, key: &str) -> Result<IPCResponse> {
    let value: Option<String> = ctx.memory.get_connection().ok().and_then(|conn| {
        conn.query_row(
            "SELECT value FROM user_facts WHERE key = ?1",
            rusqlite::params![key],
            |r| r.get(0),
        )
        .ok()
    });
    Ok(IPCResponse::UserFact {
        id: id.to_string(),
        key: key.to_string(),
        value,
    })
}

pub async fn handle_set_user_fact(
    ctx: &HandlerCtx,
    id: &str,
    key: &str,
    value: &str,
) -> Result<IPCResponse> {
    if let Ok(conn) = ctx.memory.get_connection() {
        let _ = conn.execute(
            "INSERT INTO user_facts (key, value, created_at, updated_at) VALUES (?1, ?2, strftime('%s','now'), strftime('%s','now'))
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = strftime('%s','now')",
            rusqlite::params![key, value],
        );
    }
    Ok(IPCResponse::Ok { id: id.to_string() })
}

pub async fn handle_get_memory_health(ctx: &HandlerCtx, id: &str) -> Result<IPCResponse> {
    let conn = match ctx.memory.get_connection() {
        Ok(c) => c,
        Err(_) => {
            return Ok(IPCResponse::Error {
                id: id.to_string(),
                message: "DB unavailable".to_string(),
            })
        }
    };
    let node_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
        .unwrap_or(0);
    let edge_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))
        .unwrap_or(0);
    let emb_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0))
        .unwrap_or(0);
    let pinned: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM user_facts WHERE key LIKE 'node_pinned:%' AND value = 'true'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    // Average feedback (proxy for confidence)
    let avg_conf: f64 = conn
        .query_row(
            "SELECT COALESCE(AVG(CAST(rating AS REAL)), 0.0) FROM memory_feedback",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0.0);
    // DB file size
    let db_size = std::fs::metadata(&ctx.memory.db_path)
        .map(|m| m.len())
        .unwrap_or(0);
    let last_trained_at: Option<i64> = conn.query_row(
        "SELECT last_run_at FROM training_jobs WHERE job_name = 'graphsage' AND last_run_at > 0",
        [], |r| r.get(0),
    ).ok();

    Ok(IPCResponse::MemoryHealth {
        id: id.to_string(),
        node_count,
        edge_count,
        embedding_count: emb_count,
        pinned_count: pinned,
        avg_confidence: avg_conf,
        db_size_bytes: db_size,
        gnn_available: ctx.gnn_available,
        last_trained_at,
    })
}

pub async fn handle_set_node_pinned(
    ctx: &HandlerCtx,
    id: &str,
    node_id: &str,
    pinned: bool,
) -> Result<IPCResponse> {
    let key = format!("node_pinned:{}", node_id);
    let val = if pinned { "true" } else { "false" };
    if let Ok(conn) = ctx.memory.get_connection() {
        let _ = conn.execute(
            "INSERT INTO user_facts (key, value, created_at, updated_at) VALUES (?1, ?2, strftime('%s','now'), strftime('%s','now'))
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = strftime('%s','now')",
            rusqlite::params![&key, val],
        );
    }
    Ok(IPCResponse::Ok { id: id.to_string() })
}

pub async fn handle_delete_node(ctx: &HandlerCtx, id: &str, node_id: &str) -> Result<IPCResponse> {
    if let Ok(conn) = ctx.memory.get_connection() {
        let _ = conn.execute(
            "DELETE FROM nodes WHERE id = ?1",
            rusqlite::params![node_id],
        );
    }
    Ok(IPCResponse::Ok { id: id.to_string() })
}

pub async fn handle_update_node_summary(
    ctx: &HandlerCtx,
    id: &str,
    node_id: &str,
    summary: &str,
) -> Result<IPCResponse> {
    if let Ok(conn) = ctx.memory.get_connection() {
        let _ = conn.execute(
            "UPDATE object_memory SET summary = ?1 WHERE node_id = ?2",
            rusqlite::params![summary, node_id],
        );
    }
    Ok(IPCResponse::Ok { id: id.to_string() })
}

pub async fn handle_export_memory(ctx: &HandlerCtx, id: &str) -> Result<IPCResponse> {
    let conn = match ctx.memory.get_connection() {
        Ok(c) => c,
        Err(_) => {
            return Ok(IPCResponse::Error {
                id: id.to_string(),
                message: "DB unavailable".to_string(),
            })
        }
    };

    let nodes: Vec<serde_json::Value> = conn
        .prepare("SELECT id, node_type, title, created_at, updated_at, metadata_json FROM nodes")
        .and_then(|mut s| {
            s.query_map([], |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_,String>(0)?,
                    "node_type": r.get::<_,String>(1)?,
                    "title": r.get::<_,String>(2)?,
                    "created_at": r.get::<_,i64>(3)?,
                    "updated_at": r.get::<_,i64>(4)?,
                    "metadata_json": r.get::<_,Option<String>>(5)?,
                }))
            })
            .and_then(|r| r.collect::<rusqlite::Result<Vec<_>>>())
        })
        .unwrap_or_default();

    let object_memories: Vec<serde_json::Value> = conn
        .prepare("SELECT node_id, summary, key_facts_json, tags_json FROM object_memory")
        .and_then(|mut s| {
            s.query_map([], |r| {
                Ok(serde_json::json!({
                    "node_id": r.get::<_,String>(0)?,
                    "summary": r.get::<_,Option<String>>(1)?,
                    "key_facts_json": r.get::<_,Option<String>>(2)?,
                    "tags_json": r.get::<_,Option<String>>(3)?,
                }))
            })
            .and_then(|r| r.collect::<rusqlite::Result<Vec<_>>>())
        })
        .unwrap_or_default();

    let edges: Vec<serde_json::Value> = conn
        .prepare("SELECT id, source_id, target_id, relationship, strength FROM edges")
        .and_then(|mut s| {
            s.query_map([], |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_,String>(0)?,
                    "source_id": r.get::<_,String>(1)?,
                    "target_id": r.get::<_,String>(2)?,
                    "relationship": r.get::<_,String>(3)?,
                    "strength": r.get::<_,f64>(4)?,
                }))
            })
            .and_then(|r| r.collect::<rusqlite::Result<Vec<_>>>())
        })
        .unwrap_or_default();

    let data = serde_json::json!({
        "version": 1,
        "exported_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
        "nodes": nodes,
        "object_memories": object_memories,
        "edges": edges,
    });

    Ok(IPCResponse::MemoryExport {
        id: id.to_string(),
        data,
    })
}

pub async fn handle_import_memory(
    ctx: &HandlerCtx,
    id: &str,
    data: &serde_json::Value,
    conflict: &str,
) -> Result<IPCResponse> {
    let nodes = data["nodes"].as_array().cloned().unwrap_or_default();
    let edges = data["edges"].as_array().cloned().unwrap_or_default();
    let mut imported = 0usize;
    let mut skipped = 0usize;
    let mut errors = 0usize;

    if let Ok(conn) = ctx.memory.get_connection() {
        let insert_sql = match conflict {
            "overwrite" => "INSERT OR REPLACE INTO nodes (id, node_type, title, created_at, updated_at, metadata_json) VALUES (?1,?2,?3,?4,?5,?6)",
            _ => "INSERT OR IGNORE INTO nodes (id, node_type, title, created_at, updated_at, metadata_json) VALUES (?1,?2,?3,?4,?5,?6)",
        };
        for n in &nodes {
            let nid = n["id"].as_str().unwrap_or("");
            let ntype = n["node_type"].as_str().unwrap_or("concept");
            let title = n["title"].as_str().unwrap_or("");
            let ca = n["created_at"].as_i64().unwrap_or(0);
            let ua = n["updated_at"].as_i64().unwrap_or(0);
            let meta = n["metadata_json"].as_str().map(|s| s.to_string());
            match conn.execute(
                insert_sql,
                rusqlite::params![nid, ntype, title, ca, ua, meta],
            ) {
                Ok(0) => skipped += 1,
                Ok(_) => imported += 1,
                Err(_) => errors += 1,
            }
        }
        for e in &edges {
            let _ = conn.execute(
                "INSERT OR IGNORE INTO edges (id, source_id, target_id, relationship, strength) VALUES (?1,?2,?3,?4,?5)",
                rusqlite::params![
                    e["id"].as_str().unwrap_or(""),
                    e["source_id"].as_str().unwrap_or(""),
                    e["target_id"].as_str().unwrap_or(""),
                    e["relationship"].as_str().unwrap_or("relates_to"),
                    e["strength"].as_f64().unwrap_or(1.0),
                ],
            );
        }
    }

    Ok(IPCResponse::MemoryImportResult {
        id: id.to_string(),
        imported,
        skipped,
        errors,
    })
}

pub async fn handle_log_tool_audit(
    ctx: &HandlerCtx,
    id: &str,
    tool_name: &str,
    args_json: &str,
    result_summary: &str,
    conversation_id: Option<&str>,
) -> Result<IPCResponse> {
    if let Ok(conn) = ctx.memory.get_connection() {
        let _ = conn.execute(
            "INSERT INTO tool_audit (id, timestamp, tool_name, args_json, result_summary, conversation_id)
             VALUES (?1, strftime('%s','now'), ?2, ?3, ?4, ?5)",
            rusqlite::params![
                uuid::Uuid::new_v4().to_string(),
                tool_name,
                &args_json[..args_json.len().min(500)],
                &result_summary[..result_summary.len().min(500)],
                conversation_id,
            ],
        );
        // Keep only the most recent 1 000 rows to prevent unbounded growth.
        let _ = conn.execute(
            "DELETE FROM tool_audit WHERE id NOT IN \
             (SELECT id FROM tool_audit ORDER BY timestamp DESC LIMIT 1000)",
            [],
        );
    }
    Ok(IPCResponse::Ok { id: id.to_string() })
}

pub async fn handle_get_tool_audit_log(
    ctx: &HandlerCtx,
    id: &str,
    limit: Option<usize>,
) -> Result<IPCResponse> {
    let lim = limit.unwrap_or(50) as i64;
    let entries: Vec<serde_json::Value> = ctx
        .memory
        .get_connection()
        .ok()
        .and_then(|conn| {
            conn.prepare(
                "SELECT id, timestamp, tool_name, args_json, result_summary, conversation_id
             FROM tool_audit ORDER BY timestamp DESC LIMIT ?1",
            )
            .and_then(|mut s| {
                s.query_map(rusqlite::params![lim], |r| {
                    Ok(serde_json::json!({
                        "id":              r.get::<_,String>(0)?,
                        "timestamp":       r.get::<_,i64>(1)?,
                        "tool_name":       r.get::<_,String>(2)?,
                        "args_json":       r.get::<_,Option<String>>(3)?,
                        "result_summary":  r.get::<_,Option<String>>(4)?,
                        "conversation_id": r.get::<_,Option<String>>(5)?,
                    }))
                })
                .and_then(|r| r.collect::<rusqlite::Result<Vec<_>>>())
            })
            .ok()
        })
        .unwrap_or_default();
    Ok(IPCResponse::ToolAuditLog {
        id: id.to_string(),
        entries,
    })
}

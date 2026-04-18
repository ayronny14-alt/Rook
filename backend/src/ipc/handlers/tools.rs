use anyhow::Result;
use uuid::Uuid;

use super::HandlerCtx;
use crate::ipc::protocol::{IPCResponse, PendingEditDiff};
use crate::memory::graph::GraphMemory;
use crate::memory::object::ObjectMemoryStore;

pub async fn handle_read_file(
    ctx: &HandlerCtx,
    id: &str,
    path: &str,
    conversation_id: Option<&str>,
) -> Result<IPCResponse> {
    let content = ctx.tools.read_file(path).await?;

    {
        let mem_bg = ctx.memory.clone();
        let path_bg = path.to_string();
        let summary = content.chars().take(200).collect::<String>();
        let conv_tag = conversation_id.map(|s| s.to_string());
        tokio::spawn(async move {
            let graph = GraphMemory::new(mem_bg.clone());
            let already = graph
                .search_nodes(Some(crate::memory::graph::NodeType::File), Some(&path_bg))
                .ok()
                .map(|v| !v.is_empty())
                .unwrap_or(false);
            if !already {
                let mut meta = serde_json::json!({ "source": "file_read" });
                if let Some(cid) = conv_tag {
                    meta["conversation_id"] = serde_json::json!(cid);
                }
                if let Ok(node) =
                    graph.create_node(crate::memory::graph::NodeType::File, &path_bg, Some(meta))
                {
                    let obj_store = ObjectMemoryStore::new(mem_bg);
                    let mut obj = ObjectMemoryStore::create_for_node(&node.id);
                    obj.summary = Some(summary);
                    let _ = obj_store.upsert(&node.id, &obj);
                }
            }
        });
    }

    Ok(IPCResponse::FileContent {
        id: id.to_string(),
        path: path.to_string(),
        content,
    })
}

pub async fn handle_write_file(
    ctx: &HandlerCtx,
    id: &str,
    path: &str,
    content: &str,
) -> Result<IPCResponse> {
    ctx.tools.write_file(path, content).await?;
    Ok(IPCResponse::FileWritten {
        id: id.to_string(),
        path: path.to_string(),
        success: true,
    })
}

pub async fn handle_web_search(
    ctx: &HandlerCtx,
    id: &str,
    query: &str,
    conversation_id: Option<&str>,
) -> Result<IPCResponse> {
    let results = ctx.tools.web_search(query).await?;

    {
        let mem_bg = ctx.memory.clone();
        let results_bg = results.clone();
        let query_bg = query.to_string();
        let conv_tag = conversation_id.map(|s| s.to_string());
        tokio::spawn(async move {
            let graph = GraphMemory::new(mem_bg.clone());
            let obj_store = ObjectMemoryStore::new(mem_bg);
            for result in results_bg.iter().take(3) {
                let title = result
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&query_bg);
                let snippet = result.get("snippet").and_then(|v| v.as_str()).unwrap_or("");
                let url = result.get("url").and_then(|v| v.as_str()).unwrap_or("");
                if title.is_empty() {
                    continue;
                }
                let already = graph
                    .search_nodes(Some(crate::memory::graph::NodeType::Website), Some(title))
                    .ok()
                    .map(|v| !v.is_empty())
                    .unwrap_or(false);
                if already {
                    continue;
                }
                let mut meta = serde_json::json!({ "url": url, "source": "web_search" });
                if let Some(ref cid) = conv_tag {
                    meta["conversation_id"] = serde_json::json!(cid);
                }
                if let Ok(node) =
                    graph.create_node(crate::memory::graph::NodeType::Website, title, Some(meta))
                {
                    let mut obj = ObjectMemoryStore::create_for_node(&node.id);
                    obj.summary = Some(snippet.chars().take(200).collect());
                    let _ = obj_store.upsert(&node.id, &obj);
                }
            }
        });
    }

    Ok(IPCResponse::WebSearchResults {
        id: id.to_string(),
        results,
    })
}

pub async fn handle_spawn_browser(
    ctx: &HandlerCtx,
    id: &str,
    headless: bool,
) -> Result<IPCResponse> {
    let url = ctx.tools.spawn_browser(headless).await?;
    Ok(IPCResponse::SpawnBrowserResult {
        id: id.to_string(),
        debugging_url: url,
    })
}

pub async fn handle_spawn_cdp_browser(
    ctx: &HandlerCtx,
    id: &str,
    headless: bool,
) -> Result<IPCResponse> {
    let url = ctx.tools.spawn_cdp_browser(headless).await?;
    Ok(IPCResponse::SpawnCdpBrowserResult {
        id: id.to_string(),
        debugging_url: url,
    })
}

pub async fn handle_cdp_navigate(ctx: &HandlerCtx, id: &str, url: &str) -> Result<IPCResponse> {
    let content = ctx.tools.cdp_navigate(url).await?;
    Ok(IPCResponse::CdpNavigateResult {
        id: id.to_string(),
        content,
    })
}

pub async fn handle_cdp_click(ctx: &HandlerCtx, id: &str, selector: &str) -> Result<IPCResponse> {
    ctx.tools
        .cdp_click(selector)
        .await
        .map(|_| IPCResponse::CdpActionResult {
            id: id.to_string(),
            success: true,
            details: None,
        })
        .or_else(|e| {
            Ok(IPCResponse::CdpActionResult {
                id: id.to_string(),
                success: false,
                details: Some(serde_json::json!(e.to_string())),
            })
        })
}

pub async fn handle_cdp_type(
    ctx: &HandlerCtx,
    id: &str,
    selector: &str,
    text: &str,
) -> Result<IPCResponse> {
    ctx.tools
        .cdp_type(selector, text)
        .await
        .map(|_| IPCResponse::CdpActionResult {
            id: id.to_string(),
            success: true,
            details: None,
        })
        .or_else(|e| {
            Ok(IPCResponse::CdpActionResult {
                id: id.to_string(),
                success: false,
                details: Some(serde_json::json!(e.to_string())),
            })
        })
}

pub async fn handle_cdp_evaluate(ctx: &HandlerCtx, id: &str, js: &str) -> Result<IPCResponse> {
    match ctx.tools.cdp_evaluate(js).await {
        Ok(res) => Ok(IPCResponse::CdpActionResult {
            id: id.to_string(),
            success: true,
            details: Some(serde_json::json!(res)),
        }),
        Err(e) => Ok(IPCResponse::CdpActionResult {
            id: id.to_string(),
            success: false,
            details: Some(serde_json::json!(e.to_string())),
        }),
    }
}

pub async fn handle_screenshot(ctx: &HandlerCtx, id: &str, full: bool) -> Result<IPCResponse> {
    match ctx.tools.cdp_screenshot_base64(full).await {
        Ok(b64) => Ok(IPCResponse::ScreenshotResult {
            id: id.to_string(),
            image_base64: b64,
        }),
        Err(e) => Ok(IPCResponse::Error {
            id: id.to_string(),
            message: e.to_string(),
        }),
    }
}

pub async fn handle_kill_browser(ctx: &HandlerCtx, id: &str) -> Result<IPCResponse> {
    let _ = ctx.tools.kill_browser().await;
    let _ = ctx.tools.kill_cdp_browser().await;
    Ok(IPCResponse::CdpActionResult {
        id: id.to_string(),
        success: true,
        details: None,
    })
}

pub async fn handle_browser_debugging_url(ctx: &HandlerCtx, id: &str) -> Result<IPCResponse> {
    let url = ctx
        .tools
        .cdp_debugging_url()
        .await
        .or(ctx.tools.browser_debugging_url().await);
    Ok(IPCResponse::BrowserDebuggingUrl {
        id: id.to_string(),
        url,
    })
}

pub async fn handle_execute_skill(
    ctx: &HandlerCtx,
    id: &str,
    skill_name: &str,
    args: serde_json::Value,
) -> Result<IPCResponse> {
    let result = ctx.skills.execute(skill_name, args).await?;
    Ok(IPCResponse::SkillExecuted {
        id: id.to_string(),
        result,
    })
}

pub async fn handle_list_skills(ctx: &HandlerCtx, id: &str) -> Result<IPCResponse> {
    let skill_list = ctx.skills.list();
    let skills_json: Vec<serde_json::Value> = skill_list
        .into_iter()
        .filter_map(|s| serde_json::to_value(&s).ok())
        .collect();
    Ok(IPCResponse::SkillsList {
        id: id.to_string(),
        skills: skills_json,
    })
}

pub async fn handle_get_previews(id: &str) -> Result<IPCResponse> {
    Ok(IPCResponse::Previews {
        id: id.to_string(),
        previews: vec![],
    })
}

pub async fn handle_create_preview(id: &str, name: &str, content: &str) -> Result<IPCResponse> {
    let previews_dir = std::env::current_dir()?.join(".docu").join("previews");
    std::fs::create_dir_all(&previews_dir)?;
    let path = previews_dir.join(format!("{}.html", name));
    std::fs::write(&path, content)?;
    Ok(IPCResponse::PreviewCreated {
        id: id.to_string(),
        name: name.to_string(),
        path: path.to_string_lossy().to_string(),
    })
}

pub async fn handle_approve_pending_tools(
    ctx: &HandlerCtx,
    id: &str,
    conversation_id: &str,
    approved: bool,
) -> Result<IPCResponse> {
    if !approved {
        if let Ok(mut pt) = ctx.pending_tools.lock() {
            pt.remove(conversation_id);
        }
        return Ok(IPCResponse::Chat {
            id: id.to_string(),
            conversation_id: conversation_id.to_string(),
            content: "Changes rejected.".to_string(),
            tool_calls: None,
            context_packet: None,
            usage: None,
        });
    }

    let diffs_opt = ctx
        .pending_tools
        .lock()
        .ok()
        .and_then(|mut pt| pt.remove(conversation_id));
    let diffs: Vec<PendingEditDiff> = match diffs_opt {
        Some(d) => d,
        None => {
            return Ok(IPCResponse::Chat {
                id: id.to_string(),
                conversation_id: conversation_id.to_string(),
                content: "No pending changes found for this conversation.".to_string(),
                tool_calls: None,
                context_packet: None,
                usage: None,
            });
        }
    };

    let mut results: Vec<String> = Vec::new();
    for diff in &diffs {
        let result = match diff.tool_name.as_str() {
            "code_edit" => {
                let path = diff.args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let action = diff
                    .args
                    .get("action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match action {
                    "search_replace" => {
                        let search = diff
                            .args
                            .get("search")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let replace_text = diff
                            .args
                            .get("replace")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        match tokio::fs::read_to_string(path).await {
                            Ok(original) => {
                                let modified = original.replacen(search, replace_text, 1);
                                match tokio::fs::write(path, &modified).await {
                                    Ok(_) => format!("Applied edit to {}", path),
                                    Err(e) => format!("Error writing {}: {}", path, e),
                                }
                            }
                            Err(e) => format!("Error reading {}: {}", path, e),
                        }
                    }
                    "append" => {
                        let content = diff
                            .args
                            .get("content")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        match tokio::fs::read_to_string(path).await {
                            Ok(original) => {
                                let modified =
                                    format!("{}\n{}", original.trim_end_matches('\n'), content);
                                match tokio::fs::write(path, &modified).await {
                                    Ok(_) => format!("Appended to {}", path),
                                    Err(e) => format!("Error writing {}: {}", path, e),
                                }
                            }
                            Err(e) => format!("Error reading {}: {}", path, e),
                        }
                    }
                    "create" => {
                        let content = diff
                            .args
                            .get("content")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if let Some(parent) = std::path::Path::new(path).parent() {
                            let _ = tokio::fs::create_dir_all(parent).await;
                        }
                        match tokio::fs::write(path, content).await {
                            Ok(_) => format!("Created {}", path),
                            Err(e) => format!("Error creating {}: {}", path, e),
                        }
                    }
                    _ => format!("Unknown code_edit action '{}' on {}", action, path),
                }
            }
            "terminal_execute" | "execute_command" => {
                let command = diff
                    .args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match ctx.tools.terminal_execute(command).await {
                    Ok(output) => output,
                    Err(e) => format!("Error: {}", e),
                }
            }
            other => format!("Unsupported tool '{}' in approval flow", other),
        };
        results.push(result);
    }

    let summary = results.join("\n");
    if let Ok(conn) = ctx.memory.get_connection() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let msg_id = Uuid::new_v4().to_string();
        let _ = conn.execute(
            "INSERT INTO messages (id, conversation_id, role, content, created_at) VALUES (?1, ?2, 'assistant', ?3, ?4)",
            rusqlite::params![&msg_id, conversation_id, &summary, now],
        );
    }

    Ok(IPCResponse::Chat {
        id: id.to_string(),
        conversation_id: conversation_id.to_string(),
        content: summary,
        tool_calls: None,
        context_packet: None,
        usage: None,
    })
}

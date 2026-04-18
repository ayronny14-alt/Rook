use anyhow::Result;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::ipc::handlers::{
    admin::{
        handle_delete_node, handle_export_memory, handle_get_memory_health,
        handle_get_tool_audit_log, handle_get_user_fact, handle_graceful_shutdown,
        handle_import_memory, handle_log_tool_audit, handle_set_node_pinned, handle_set_user_fact,
        handle_update_node_summary,
    },
    chat::handle_chat,
    conversations::{
        handle_delete_conversation, handle_get_conversation_messages, handle_get_conversations,
        handle_pin_conversation, handle_regenerate_last_message, handle_rename_conversation,
        handle_search_conversations,
    },
    git::{
        handle_git_branch, handle_git_commit, handle_git_diff, handle_git_log, handle_git_status,
    },
    memory::{
        handle_create_node, handle_get_connected_nodes, handle_get_node, handle_query_memory,
        handle_search_embeddings, handle_submit_memory_feedback,
    },
    plugins::{
        handle_install_plugin, handle_launch_gemma, handle_list_plugins, handle_search_plugins,
        handle_set_plugin_enabled, handle_start_mcp, handle_stop_mcp, handle_uninstall_plugin,
        handle_update_config,
    },
    tools::{
        handle_approve_pending_tools, handle_browser_debugging_url, handle_cdp_click,
        handle_cdp_evaluate, handle_cdp_navigate, handle_cdp_type, handle_create_preview,
        handle_execute_skill, handle_get_previews, handle_kill_browser, handle_list_skills,
        handle_read_file, handle_screenshot, handle_spawn_browser, handle_spawn_cdp_browser,
        handle_web_search, handle_write_file,
    },
    HandlerCtx,
};
use crate::ipc::protocol::{IPCRequest, IPCResponse, PendingEditDiff};
use crate::llm::client::LLMClient;
use crate::llm::orchestrator::GemmaOrchestrator;
use crate::llm::types::LLMConfig;
use crate::memory::storage::MemoryStorage;
use crate::plugins::mcp_runner::McpManager;
use crate::plugins::registry::PluginRegistry;
use crate::skills::registry::SkillsRegistry;
use crate::tools::ToolExecutor;

const PIPE_NAME: &str = r"\\.\pipe\rook";

pub struct IPCServer {
    memory: MemoryStorage,
    llm: LLMClient,
    skills: SkillsRegistry,
    tools: ToolExecutor,
    project_instructions: Option<String>,
    cancellations: Arc<Mutex<HashMap<String, CancellationToken>>>,
    pending_tools: Arc<Mutex<HashMap<String, Vec<PendingEditDiff>>>>,
    plugin_registry: Arc<PluginRegistry>,
    mcp_manager: Arc<Mutex<McpManager>>,
    gemma: Arc<GemmaOrchestrator>,
    config_override: Arc<Mutex<Option<LLMConfig>>>,
    gnn_available: bool,
}

impl IPCServer {
    pub fn new(
        memory: MemoryStorage,
        llm: LLMClient,
        skills: SkillsRegistry,
        tools: ToolExecutor,
    ) -> Self {
        let project_instructions = Self::load_project_instructions();
        let plugin_registry = Arc::new(PluginRegistry::new(memory.clone()));
        let gnn_available = crate::memory::gnn::is_gnn_available();
        Self {
            memory,
            llm,
            skills,
            tools,
            project_instructions,
            cancellations: Arc::new(Mutex::new(HashMap::new())),
            pending_tools: Arc::new(Mutex::new(HashMap::new())),
            plugin_registry,
            mcp_manager: Arc::new(Mutex::new(McpManager::new())),
            gemma: Arc::new(GemmaOrchestrator::from_env()),
            config_override: Arc::new(Mutex::new(None)),
            gnn_available,
        }
    }

    fn load_project_instructions() -> Option<String> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let candidates = [cwd.join("ROOK.md"), cwd.join("AGENTS.md")];
        for path in &candidates {
            if path.exists() {
                if let Ok(text) = std::fs::read_to_string(path) {
                    if !text.trim().is_empty() {
                        info!("Loaded project instructions from {:?}", path);
                        return Some(text);
                    }
                }
            }
        }
        None
    }

    pub async fn start(&self) -> Result<()> {
        info!("Starting IPC server on {}", PIPE_NAME);
        loop {
            if let Err(e) = self.accept_connection().await {
                error!("Connection error: {}", e);
            }
        }
    }

    async fn accept_connection(&self) -> Result<()> {
        let pipe = tokio::net::windows::named_pipe::ServerOptions::new().create(PIPE_NAME)?;
        info!("Named pipe created, waiting for connection...");
        pipe.connect().await?;
        info!("Client connected to named pipe");

        let memory = self.memory.clone();
        let llm = self.llm.clone();
        let skills = self.skills.clone();
        let tools = self.tools.clone();
        let project_instructions = self.project_instructions.clone();
        let cancellations = self.cancellations.clone();
        let pending_tools = self.pending_tools.clone();
        let plugin_registry = self.plugin_registry.clone();
        let mcp_manager = self.mcp_manager.clone();
        let gemma = self.gemma.clone();
        let config_override = self.config_override.clone();
        let gnn_available = self.gnn_available;

        tokio::spawn(async move {
            if let Err(e) = Self::handle_connection(
                pipe,
                memory,
                llm,
                skills,
                tools,
                project_instructions,
                cancellations,
                pending_tools,
                plugin_registry,
                mcp_manager,
                gemma,
                config_override,
                gnn_available,
            )
            .await
            {
                error!("Error handling connection: {}", e);
            }
            info!("Client disconnected");
        });

        Ok(())
    }

    async fn handle_connection(
        pipe: tokio::net::windows::named_pipe::NamedPipeServer,
        memory: MemoryStorage,
        llm: LLMClient,
        skills: SkillsRegistry,
        tools: ToolExecutor,
        project_instructions: Option<String>,
        cancellations: Arc<Mutex<HashMap<String, CancellationToken>>>,
        pending_tools: Arc<Mutex<HashMap<String, Vec<PendingEditDiff>>>>,
        plugin_registry: Arc<PluginRegistry>,
        mcp_manager: Arc<Mutex<McpManager>>,
        gemma: Arc<GemmaOrchestrator>,
        config_override: Arc<Mutex<Option<LLMConfig>>>,
        gnn_available: bool,
    ) -> Result<()> {
        let (reader, writer) = tokio::io::split(pipe);
        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();

        // ONE shared writer task for the whole connection. All responses
        // (streaming chunks, final results, cancel acks) go through this
        // single mpsc channel so the main read-loop never blocks on I/O.
        let (write_tx, mut write_rx) = tokio::sync::mpsc::channel::<IPCResponse>(64);
        let _writer_task = tokio::spawn(async move {
            let mut w = writer;
            while let Some(resp) = write_rx.recv().await {
                if let Ok(json) = serde_json::to_string(&resp) {
                    if w.write_all(json.as_bytes()).await.is_err() {
                        break;
                    }
                    if w.write_all(b"\n").await.is_err() {
                        break;
                    }
                    if w.flush().await.is_err() {
                        break;
                    }
                }
            }
        });

        // Simple sliding-window rate limiter: max 60 non-streaming messages per
        // second per connection. Prevents a runaway renderer from flooding the
        // backend. Cancel and chunk-free messages are cheap; streaming chat
        // requests are naturally throttled by LLM latency.
        const RATE_LIMIT: usize = 60;
        const RATE_WINDOW_MS: u128 = 1_000;
        let mut rate_window_start = std::time::Instant::now();
        let mut rate_count: usize = 0;

        loop {
            line.clear();
            let bytes_read = buf_reader.read_line(&mut line).await?;
            if bytes_read == 0 {
                break;
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Rate-limit check: reset counter each second, reject if over limit.
            let now = std::time::Instant::now();
            if now.duration_since(rate_window_start).as_millis() >= RATE_WINDOW_MS {
                rate_window_start = now;
                rate_count = 0;
            }
            rate_count += 1;
            if rate_count > RATE_LIMIT {
                warn!(
                    "IPC rate limit exceeded ({} msg/s) — dropping message",
                    rate_count
                );
                let _ = write_tx
                    .send(IPCResponse::Error {
                        id: "rate-limited".to_string(),
                        message: "Rate limit exceeded — slow down requests".to_string(),
                    })
                    .await;
                continue;
            }

            let raw_type = {
                serde_json::from_str::<serde_json::Value>(trimmed)
                    .ok()
                    .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string))
                    .unwrap_or_else(|| "<unparseable>".to_string())
            };
            info!("IPC request received: type={}", raw_type);

            let request: IPCRequest = match serde_json::from_str(trimmed) {
                Ok(r) => r,
                Err(e) => {
                    error!(
                        "IPC parse error for type='{}': {} | raw={}",
                        raw_type,
                        e,
                        &trimmed[..trimmed.len().min(200)]
                    );
                    let _ = write_tx
                        .send(IPCResponse::Error {
                            id: "unknown".to_string(),
                            message: format!("Invalid JSON: {}", e),
                        })
                        .await;
                    continue;
                }
            };

            let id = extract_request_id(&request);

            // Handle cancel inline — must return immediately while dispatch runs
            if let IPCRequest::CancelRequest { id: cid, target_id } = &request {
                let token_opt = cancellations
                    .lock()
                    .ok()
                    .and_then(|mut m| m.remove(target_id));
                if let Some(token) = token_opt {
                    token.cancel();
                    info!("Cancel signal sent to request {}", target_id);
                } else {
                    warn!("Cancel received for unknown target_id={}", target_id);
                }
                let _ = write_tx
                    .send(IPCResponse::Cancelled {
                        id: cid.clone(),
                        target_id: target_id.clone(),
                    })
                    .await;
                continue;
            }

            let cancel_token = CancellationToken::new();
            if let Ok(mut map) = cancellations.lock() {
                map.insert(id.clone(), cancel_token.clone());
            } else {
                warn!(
                    "cancellations mutex poisoned — request {} will not be cancellable",
                    id
                );
            }

            // Spawn dispatch in a background task so the main loop can
            // immediately go back to reading the next request (critically,
            // so it can read cancel_request while a chat is streaming).
            let ctx = HandlerCtx {
                memory: memory.clone(),
                llm: llm.clone(),
                skills: skills.clone(),
                tools: tools.clone(),
                project_instructions: project_instructions.clone(),
                pending_tools: pending_tools.clone(),
                plugin_registry: plugin_registry.clone(),
                mcp_manager: mcp_manager.clone(),
                gemma: gemma.clone(),
                config_override: config_override.clone(),
                chunk_tx: write_tx.clone(),
                cancel_token,
                gnn_available,
            };
            let write_tx_done = write_tx.clone();
            let cancellations_done = cancellations.clone();
            let id_done = id.clone();
            let raw_type_done = raw_type.clone();
            tokio::spawn(async move {
                let result = dispatch(&request, &ctx).await;
                let response = match result {
                    Ok(resp) => resp,
                    Err(e) => {
                        error!("Handler error for type='{}': {}", raw_type_done, e);
                        IPCResponse::Error {
                            id: id_done.clone(),
                            message: e.to_string(),
                        }
                    }
                };
                // Drop ctx's chunk_tx clone first so all chunk sends have landed in the channel ahead of the final response.
                drop(ctx);
                let _ = write_tx_done.send(response).await;
                if let Ok(mut map) = cancellations_done.lock() {
                    map.remove(&id_done);
                }
                info!("IPC response sent: type={}", raw_type_done);
            });
        }

        Ok(())
    }
}

/// Route an IPC request to the appropriate handler.
async fn dispatch(request: &IPCRequest, ctx: &HandlerCtx) -> Result<IPCResponse> {
    match request {
        IPCRequest::HealthCheck { id } => {
            let effective_cfg = ctx
                .config_override
                .lock()
                .ok()
                .and_then(|g| g.clone())
                .unwrap_or_else(|| ctx.llm.base_config().clone());
            let mock_mode = effective_cfg.api_key.trim().is_empty();
            Ok(IPCResponse::HealthCheck {
                id: id.clone(),
                status: "ok".to_string(),
                mock_mode,
                model: effective_cfg.model.clone(),
            })
        }

        IPCRequest::GetMemoryCapabilities { id } => {
            let manager = crate::memory::gnn::GraphTrainingManager::new(ctx.memory.clone());
            let stats = manager
                .stats()
                .unwrap_or(crate::memory::gnn::GraphTrainingStats {
                    nodes: 0,
                    edges: 0,
                    embeddings: 0,
                    ready: false,
                });
            let last_trained_at: Option<i64> = ctx.memory.get_connection().ok().and_then(|conn| {
                conn.query_row(
                    "SELECT last_run_at FROM training_jobs WHERE job_name = 'graphsage' AND last_run_at > 0",
                    [],
                    |row| row.get(0),
                ).ok()
            });
            Ok(IPCResponse::MemoryCapabilities {
                id: id.clone(),
                gnn_available: ctx.gnn_available,
                last_trained_at,
                node_count: stats.nodes,
                edge_count: stats.edges,
                embedding_count: stats.embeddings,
            })
        }

        IPCRequest::Chat {
            id,
            conversation_id,
            message,
            agent_mode,
            model,
            images: _,
            system_prompt,
            temperature,
            max_tokens,
        } => {
            handle_chat(
                ctx,
                id,
                conversation_id.as_deref(),
                message,
                agent_mode.as_deref(),
                model.as_deref(),
                system_prompt.as_deref(),
                *temperature,
                *max_tokens,
            )
            .await
        }

        IPCRequest::CreateNode {
            id,
            node_type,
            title,
            metadata,
        } => handle_create_node(ctx, id, node_type, title, metadata.clone()).await,

        IPCRequest::QueryMemory {
            id,
            query,
            node_type,
            limit,
        } => handle_query_memory(ctx, id, query, node_type.as_deref(), *limit).await,

        IPCRequest::GetNode { id, node_id } => handle_get_node(ctx, id, node_id).await,

        IPCRequest::GetConnectedNodes {
            id,
            node_id,
            relationship,
        } => handle_get_connected_nodes(ctx, id, node_id, relationship.as_deref()).await,

        IPCRequest::SearchEmbeddings { id, query, limit } => {
            handle_search_embeddings(ctx, id, query, *limit).await
        }

        IPCRequest::SubmitMemoryFeedback {
            id,
            node_id,
            query,
            rating,
            reason,
            source,
            session_id,
        } => {
            handle_submit_memory_feedback(
                ctx,
                id,
                node_id,
                query.as_deref(),
                rating,
                reason.as_deref(),
                source.as_deref(),
                session_id.as_deref(),
            )
            .await
        }

        IPCRequest::ReadFile {
            id,
            path,
            conversation_id,
        } => handle_read_file(ctx, id, path, conversation_id.as_deref()).await,

        IPCRequest::WriteFile { id, path, content } => {
            handle_write_file(ctx, id, path, content).await
        }

        IPCRequest::WebSearch {
            id,
            query,
            conversation_id,
        } => handle_web_search(ctx, id, query, conversation_id.as_deref()).await,

        IPCRequest::SpawnBrowser { id, headless } => handle_spawn_browser(ctx, id, *headless).await,

        IPCRequest::SpawnCdpBrowser { id, headless } => {
            handle_spawn_cdp_browser(ctx, id, *headless).await
        }

        IPCRequest::CdpNavigate { id, url } => handle_cdp_navigate(ctx, id, url).await,

        IPCRequest::CdpClick { id, selector } => handle_cdp_click(ctx, id, selector).await,

        IPCRequest::CdpType { id, selector, text } => {
            handle_cdp_type(ctx, id, selector, text).await
        }

        IPCRequest::CdpEvaluate { id, js } => handle_cdp_evaluate(ctx, id, js).await,

        IPCRequest::Screenshot {
            id,
            selector: _,
            full,
        } => handle_screenshot(ctx, id, full.unwrap_or(true)).await,

        IPCRequest::KillBrowser { id } => handle_kill_browser(ctx, id).await,

        IPCRequest::BrowserDebuggingUrl { id } => handle_browser_debugging_url(ctx, id).await,

        IPCRequest::ExecuteSkill {
            id,
            skill_name,
            args,
        } => handle_execute_skill(ctx, id, skill_name, args.clone()).await,

        IPCRequest::ListSkills { id } => handle_list_skills(ctx, id).await,

        IPCRequest::GetPreviews { id } => handle_get_previews(id).await,

        IPCRequest::CreatePreview { id, name, content } => {
            handle_create_preview(id, name, content).await
        }

        IPCRequest::ApprovePendingTools {
            id,
            conversation_id,
            approved,
        } => handle_approve_pending_tools(ctx, id, conversation_id, *approved).await,

        IPCRequest::GitStatus { id, path } => handle_git_status(ctx, id, path.as_deref()).await,

        IPCRequest::GitLog { id, n } => handle_git_log(ctx, id, n.unwrap_or(10)).await,

        IPCRequest::GitDiff { id, file_path } => {
            handle_git_diff(ctx, id, file_path.as_deref()).await
        }

        IPCRequest::GitBranch { id } => handle_git_branch(ctx, id).await,

        IPCRequest::GitCommit { id, message, files } => {
            handle_git_commit(ctx, id, message, files).await
        }

        IPCRequest::SearchPlugins { id, query } => handle_search_plugins(ctx, id, query).await,

        IPCRequest::ListPlugins { id, plugin_type } => {
            handle_list_plugins(ctx, id, plugin_type.as_deref()).await
        }

        IPCRequest::InstallPlugin {
            id,
            plugin_id,
            name,
            description,
            plugin_type,
            repo_url,
            owner,
            repo,
            stars,
        } => {
            handle_install_plugin(
                ctx,
                id,
                plugin_id,
                name.as_deref(),
                description.as_deref(),
                plugin_type.as_deref(),
                repo_url.as_deref(),
                owner.as_deref(),
                repo.as_deref(),
                *stars,
            )
            .await
        }

        IPCRequest::SetPluginEnabled {
            id,
            plugin_id,
            enabled,
        } => handle_set_plugin_enabled(ctx, id, plugin_id, *enabled).await,

        IPCRequest::UninstallPlugin { id, plugin_id } => {
            handle_uninstall_plugin(ctx, id, plugin_id).await
        }

        IPCRequest::StartMcp { id, plugin_id } => handle_start_mcp(ctx, id, plugin_id).await,

        IPCRequest::StopMcp { id, plugin_id } => handle_stop_mcp(ctx, id, plugin_id).await,

        IPCRequest::LaunchGemma { id, model } => handle_launch_gemma(ctx, id, model).await,

        IPCRequest::UpdateConfig {
            id,
            base_url,
            api_key,
            model,
        } => handle_update_config(ctx, id, base_url, api_key, model).await,

        // Handled inline in handle_connection before dispatch is called.
        IPCRequest::CancelRequest { .. } => Ok(IPCResponse::Error {
            id: "?".to_string(),
            message: "CancelRequest handled upstream".to_string(),
        }),

        IPCRequest::GracefulShutdown { id } => handle_graceful_shutdown(ctx, id).await,

        // scheduler
        IPCRequest::ListScheduledTasks { id } => {
            crate::ipc::handlers::scheduler::handle_list_scheduled_tasks(ctx, id).await
        }
        IPCRequest::CreateScheduledTask {
            id,
            name,
            cadence,
            prompt,
            output_channel,
        } => {
            crate::ipc::handlers::scheduler::handle_create_scheduled_task(
                ctx,
                id,
                name,
                cadence,
                prompt,
                output_channel.as_deref(),
            )
            .await
        }
        IPCRequest::ApproveScheduledTask { id, task_id } => {
            crate::ipc::handlers::scheduler::handle_set_status(
                ctx,
                id,
                task_id,
                crate::scheduler::TaskStatus::Active,
                "approve",
            )
            .await
        }
        IPCRequest::CancelScheduledTask { id, task_id } => {
            crate::ipc::handlers::scheduler::handle_set_status(
                ctx,
                id,
                task_id,
                crate::scheduler::TaskStatus::Archived,
                "cancel",
            )
            .await
        }
        IPCRequest::PauseScheduledTask { id, task_id } => {
            crate::ipc::handlers::scheduler::handle_set_status(
                ctx,
                id,
                task_id,
                crate::scheduler::TaskStatus::Paused,
                "pause",
            )
            .await
        }
        IPCRequest::ResumeScheduledTask { id, task_id } => {
            crate::ipc::handlers::scheduler::handle_set_status(
                ctx,
                id,
                task_id,
                crate::scheduler::TaskStatus::Active,
                "resume",
            )
            .await
        }

        IPCRequest::GetConversations { id } => handle_get_conversations(ctx, id).await,

        IPCRequest::SearchConversations { id, query } => {
            handle_search_conversations(ctx, id, query).await
        }

        IPCRequest::RegenerateLastMessage {
            id,
            conversation_id,
            model,
        } => handle_regenerate_last_message(ctx, id, conversation_id, model.as_deref()).await,

        IPCRequest::GetConversationMessages {
            id,
            conversation_id,
        } => handle_get_conversation_messages(ctx, id, conversation_id).await,

        IPCRequest::PinConversation {
            id,
            conversation_id,
            pinned,
        } => handle_pin_conversation(ctx, id, conversation_id, *pinned).await,

        IPCRequest::SetNodePinned {
            id,
            node_id,
            pinned,
        } => handle_set_node_pinned(ctx, id, node_id, *pinned).await,

        IPCRequest::DeleteNode { id, node_id } => handle_delete_node(ctx, id, node_id).await,

        IPCRequest::UpdateNodeSummary {
            id,
            node_id,
            summary,
        } => handle_update_node_summary(ctx, id, node_id, summary).await,

        IPCRequest::GetMemoryHealth { id } => handle_get_memory_health(ctx, id).await,

        IPCRequest::ExportMemory { id } => handle_export_memory(ctx, id).await,

        IPCRequest::ImportMemory { id, data, conflict } => {
            handle_import_memory(ctx, id, data, conflict).await
        }

        IPCRequest::LogToolAudit {
            id,
            tool_name,
            args_json,
            result_summary,
            conversation_id,
        } => {
            handle_log_tool_audit(
                ctx,
                id,
                tool_name,
                args_json,
                result_summary,
                conversation_id.as_deref(),
            )
            .await
        }

        IPCRequest::GetToolAuditLog { id, limit } => {
            handle_get_tool_audit_log(ctx, id, *limit).await
        }

        IPCRequest::GetUserFact { id, key } => handle_get_user_fact(ctx, id, key).await,

        IPCRequest::SetUserFact { id, key, value } => {
            handle_set_user_fact(ctx, id, key, value).await
        }

        IPCRequest::GetSessionTodos {
            id,
            conversation_id,
        } => {
            let todos = crate::ipc::handlers::chat::get_session_todos_json(conversation_id);
            Ok(IPCResponse::SessionTodos {
                id: id.clone(),
                conversation_id: conversation_id.clone(),
                todos,
            })
        }

        IPCRequest::IndexDirectory { id, path } => {
            let dir = std::path::Path::new(path.as_str());
            if !dir.exists() || !dir.is_dir() {
                return Ok(IPCResponse::Error {
                    id: id.clone(),
                    message: format!("Not a directory: {}", path),
                });
            }
            let effective_cfg = ctx
                .config_override
                .lock()
                .ok()
                .and_then(|g| g.clone())
                .unwrap_or_else(|| ctx.llm.base_config().clone());
            let effective_llm = crate::llm::client::LLMClient::new(effective_cfg);
            let indexer = crate::indexer::file_indexer::FileIndexer::new_with_llm(
                ctx.memory.clone(),
                effective_llm,
            );
            let n = indexer.index_directory(dir).await.unwrap_or(0);
            Ok(IPCResponse::DirectoryIndexed {
                id: id.clone(),
                path: path.clone(),
                files_indexed: n,
            })
        }

        IPCRequest::DeleteConversation {
            id,
            conversation_id,
        } => handle_delete_conversation(ctx, id, conversation_id).await,

        IPCRequest::RenameConversation {
            id,
            conversation_id,
            title,
        } => handle_rename_conversation(ctx, id, conversation_id, title).await,

        IPCRequest::GetAppStats { id } => {
            let conn = match ctx.memory.get_connection() {
                Ok(c) => c,
                Err(_) => {
                    return Ok(IPCResponse::Error {
                        id: id.clone(),
                        message: "DB unavailable".to_string(),
                    })
                }
            };
            let now_unix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let day_start = now_unix - 86_400;
            let week_start = now_unix - 7 * 86_400;

            let conversation_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
                .unwrap_or(0);
            let message_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
                .unwrap_or(0);
            let user_message_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM messages WHERE role='user'", [], |r| {
                    r.get(0)
                })
                .unwrap_or(0);
            let conversations_today: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM conversations WHERE created_at > ?1",
                    rusqlite::params![day_start],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let messages_today: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM messages WHERE created_at > ?1",
                    rusqlite::params![day_start],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let conversations_this_week: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM conversations WHERE created_at > ?1",
                    rusqlite::params![week_start],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let memory_node_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
                .unwrap_or(0);
            let memory_edge_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))
                .unwrap_or(0);
            let embedding_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0))
                .unwrap_or(0);
            let plugin_installed_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM plugins WHERE status='installed'",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let plugin_enabled_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM plugins WHERE enabled=1", [], |r| {
                    r.get(0)
                })
                .unwrap_or(0);
            let tool_calls_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM tool_audit", [], |r| r.get(0))
                .unwrap_or(0);
            let oldest_conversation_at: Option<i64> = conn
                .query_row("SELECT MIN(created_at) FROM conversations", [], |r| {
                    r.get(0)
                })
                .ok()
                .flatten();
            let db_size = std::fs::metadata(&ctx.memory.db_path)
                .map(|m| m.len())
                .unwrap_or(0);

            // started_at: approximate with oldest message or now
            let started_at: i64 = conn
                .query_row("SELECT MIN(created_at) FROM messages", [], |r| r.get(0))
                .unwrap_or(now_unix);

            Ok(IPCResponse::AppStats {
                id: id.clone(),
                conversation_count,
                message_count,
                user_message_count,
                conversations_today,
                messages_today,
                conversations_this_week,
                memory_node_count,
                memory_edge_count,
                embedding_count,
                plugin_installed_count,
                plugin_enabled_count,
                tool_calls_count,
                db_size_bytes: db_size,
                started_at,
                oldest_conversation_at,
            })
        }
    }
}

fn extract_request_id(request: &IPCRequest) -> String {
    match request {
        IPCRequest::Chat { id, .. } => id.clone(),
        IPCRequest::CreateNode { id, .. } => id.clone(),
        IPCRequest::QueryMemory { id, .. } => id.clone(),
        IPCRequest::GetNode { id, .. } => id.clone(),
        IPCRequest::GetConnectedNodes { id, .. } => id.clone(),
        IPCRequest::SearchEmbeddings { id, .. } => id.clone(),
        IPCRequest::SubmitMemoryFeedback { id, .. } => id.clone(),
        IPCRequest::ReadFile { id, .. } => id.clone(),
        IPCRequest::WriteFile { id, .. } => id.clone(),
        IPCRequest::WebSearch { id, .. } => id.clone(),
        IPCRequest::SpawnBrowser { id, .. } => id.clone(),
        IPCRequest::SpawnCdpBrowser { id, .. } => id.clone(),
        IPCRequest::CdpNavigate { id, .. } => id.clone(),
        IPCRequest::CdpClick { id, .. } => id.clone(),
        IPCRequest::CdpType { id, .. } => id.clone(),
        IPCRequest::CdpEvaluate { id, .. } => id.clone(),
        IPCRequest::Screenshot { id, .. } => id.clone(),
        IPCRequest::KillBrowser { id } => id.clone(),
        IPCRequest::BrowserDebuggingUrl { id } => id.clone(),
        IPCRequest::ExecuteSkill { id, .. } => id.clone(),
        IPCRequest::ListSkills { id } => id.clone(),
        IPCRequest::GetPreviews { id } => id.clone(),
        IPCRequest::CreatePreview { id, .. } => id.clone(),
        IPCRequest::HealthCheck { id } => id.clone(),
        IPCRequest::CancelRequest { id, .. } => id.clone(),
        IPCRequest::ApprovePendingTools { id, .. } => id.clone(),
        IPCRequest::GitStatus { id, .. } => id.clone(),
        IPCRequest::GitLog { id, .. } => id.clone(),
        IPCRequest::GitDiff { id, .. } => id.clone(),
        IPCRequest::GitBranch { id } => id.clone(),
        IPCRequest::GitCommit { id, .. } => id.clone(),
        IPCRequest::SearchPlugins { id, .. } => id.clone(),
        IPCRequest::ListPlugins { id, .. } => id.clone(),
        IPCRequest::InstallPlugin { id, .. } => id.clone(),
        IPCRequest::SetPluginEnabled { id, .. } => id.clone(),
        IPCRequest::UninstallPlugin { id, .. } => id.clone(),
        IPCRequest::StartMcp { id, .. } => id.clone(),
        IPCRequest::StopMcp { id, .. } => id.clone(),
        IPCRequest::LaunchGemma { id, .. } => id.clone(),
        IPCRequest::UpdateConfig { id, .. } => id.clone(),
        IPCRequest::GetMemoryCapabilities { id } => id.clone(),
        IPCRequest::GracefulShutdown { id } => id.clone(),
        IPCRequest::ListScheduledTasks { id } => id.clone(),
        IPCRequest::CreateScheduledTask { id, .. } => id.clone(),
        IPCRequest::ApproveScheduledTask { id, .. } => id.clone(),
        IPCRequest::CancelScheduledTask { id, .. } => id.clone(),
        IPCRequest::PauseScheduledTask { id, .. } => id.clone(),
        IPCRequest::ResumeScheduledTask { id, .. } => id.clone(),
        IPCRequest::GetConversations { id } => id.clone(),
        IPCRequest::SearchConversations { id, .. } => id.clone(),
        IPCRequest::RegenerateLastMessage { id, .. } => id.clone(),
        IPCRequest::GetConversationMessages { id, .. } => id.clone(),
        IPCRequest::PinConversation { id, .. } => id.clone(),
        IPCRequest::SetNodePinned { id, .. } => id.clone(),
        IPCRequest::DeleteNode { id, .. } => id.clone(),
        IPCRequest::UpdateNodeSummary { id, .. } => id.clone(),
        IPCRequest::GetMemoryHealth { id } => id.clone(),
        IPCRequest::ExportMemory { id } => id.clone(),
        IPCRequest::ImportMemory { id, .. } => id.clone(),
        IPCRequest::LogToolAudit { id, .. } => id.clone(),
        IPCRequest::GetToolAuditLog { id, .. } => id.clone(),
        IPCRequest::GetUserFact { id, .. } => id.clone(),
        IPCRequest::SetUserFact { id, .. } => id.clone(),
        IPCRequest::GetSessionTodos { id, .. } => id.clone(),
        IPCRequest::IndexDirectory { id, .. } => id.clone(),
        IPCRequest::DeleteConversation { id, .. } => id.clone(),
        IPCRequest::RenameConversation { id, .. } => id.clone(),
        IPCRequest::GetAppStats { id } => id.clone(),
    }
}

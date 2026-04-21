pub mod admin;
pub mod chat;
pub mod conversations;
pub mod git;
pub mod memory;
pub mod plugins;
pub mod scheduler;
pub mod tool_dispatch;
pub mod tools;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

use crate::ipc::protocol::{IPCResponse, PendingEditDiff};
use crate::llm::client::LLMClient;
use crate::llm::orchestrator::GemmaOrchestrator;
use crate::llm::types::LLMConfig;
use crate::memory::storage::MemoryStorage;
use crate::plugins::mcp_runner::McpManager;
use crate::plugins::registry::PluginRegistry;
use crate::skills::registry::SkillsRegistry;
use crate::tools::ToolExecutor;

/// All shared state needed to handle a single IPC request.
pub struct HandlerCtx {
    pub memory: MemoryStorage,
    pub llm: LLMClient,
    pub skills: SkillsRegistry,
    pub tools: ToolExecutor,
    pub project_instructions: Option<String>,
    pub pending_tools: Arc<Mutex<HashMap<String, Vec<PendingEditDiff>>>>,
    pub plugin_registry: Arc<PluginRegistry>,
    pub mcp_manager: Arc<Mutex<McpManager>>,
    pub gemma: Arc<GemmaOrchestrator>,
    pub config_override: Arc<Mutex<Option<LLMConfig>>>,
    pub chunk_tx: tokio::sync::mpsc::Sender<IPCResponse>,
    pub cancel_token: CancellationToken,
    pub gnn_available: bool,
}

// No console window, please. Talk to us over the pipe or don't talk at all.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
// IPC handlers genuinely need a lot of arguments; allow instead of splitting them up.
#![allow(clippy::too_many_arguments)]
// We carry scaffolding for features that aren't wired yet - don't scream about it.
#![allow(dead_code)]
// Some handler types are complex by nature of the IPC protocol shape.
#![allow(clippy::type_complexity)]

mod computer_use;
mod error_log;
mod indexer;
mod ipc;
mod llm;
mod memory;
mod os;
mod plugins;
mod scheduler;
mod skills;
mod tools;
use llm::orchestrator::GemmaOrchestrator;

use anyhow::Result;
use tracing::{error, info};
use tracing_subscriber::prelude::*;

fn load_persisted_config() -> Option<llm::types::LLMConfig> {
    let path = dirs::data_local_dir()?.join("Rook").join("config.json");
    let json = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&json).ok()
}

#[tokio::main]
async fn main() -> Result<()> {
    // Log file: %LOCALAPPDATA%\Rook\rook.log
    let log_dir = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("Rook");
    std::fs::create_dir_all(&log_dir).ok();
    let log_path = log_dir.join("rook.log");

    // Non-blocking file appender (won't block the async runtime on slow I/O).
    let file_appender = tracing_appender::rolling::never(&log_dir, "rook.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    // Keep _guard alive for the entire process lifetime so the file writer
    // isn't flushed prematurely.
    let _ = &_guard;

    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false),
        )
        .init();

    info!("Log file: {}", log_path.display());

    info!("Rook Backend starting...");

    // Initialize the memory system
    let memory_system = memory::storage::MemoryStorage::new()?;
    info!("Memory system initialized");

    // Phase 0 honesty: report the real state of the GNN pipeline at boot
    // The graph-aware ranker silently falls back to lexical+raw embeddings when
    // the Python trainer can't run. Logging this once per boot makes the
    // failure mode visible instead of pretending GraphSAGE is always on.
    {
        let manager = memory::gnn::GraphTrainingManager::new(memory_system.clone());
        let stats = manager.stats().unwrap_or(memory::gnn::GraphTrainingStats {
            nodes: 0,
            edges: 0,
            embeddings: 0,
            ready: false,
        });
        let trained = manager.load_embeddings().map(|m| m.len()).unwrap_or(0);
        let script_path = std::env::var("ROOK_GNN_SCRIPT")
            .ok()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("gnn").join("train_graphsage.py"));
        let script_present = script_path.exists();
        let python_bin = std::env::var("ROOK_GNN_PYTHON").unwrap_or_else(|_| "python".to_string());
        let python_present = crate::os::hide(&mut std::process::Command::new(&python_bin))
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        info!(
            "GNN pipeline: nodes={} edges={} embeddings={} ready={} trained_embeddings={} script={} python({})={}",
            stats.nodes, stats.edges, stats.embeddings, stats.ready, trained,
            if script_present { "found" } else { "MISSING" },
            python_bin,
            if python_present { "ok" } else { "MISSING" },
        );
        if !script_present || !python_present {
            tracing::warn!(
                "GNN graph-aware ranking is in FALLBACK mode (no trained embeddings). \
                 Install Python + torch and ensure {} exists to enable GraphSAGE.",
                script_path.display()
            );
        }
    }

    // Initialize the LLM client - merge env defaults with any user-saved overrides
    let llm_config = {
        let mut cfg = llm::types::LLMConfig::from_env();
        if let Some(saved) = load_persisted_config() {
            // only overwrite fields the user actually set; empty strings stay as env defaults
            if !saved.api_key.trim().is_empty() {
                cfg.api_key = saved.api_key;
            }
            if !saved.base_url.trim().is_empty() {
                cfg.base_url = saved.base_url;
            }
            if !saved.model.trim().is_empty() {
                cfg.model = saved.model;
            }
            if !saved.embedding_api_key.trim().is_empty() {
                cfg.embedding_api_key = saved.embedding_api_key;
            }
        }
        cfg
    };
    let llm_client = llm::client::LLMClient::new(llm_config);
    info!("LLM client initialized");

    // Gemma orchestrator: may start a local Ollama/Gemma backend if resources allow
    let gemma = GemmaOrchestrator::from_env();
    if gemma.is_auto_start() {
        match gemma.try_start().await {
            Ok(true) => info!("Gemma orchestrator started or verified process."),
            Ok(false) => info!("Gemma auto-start skipped due to insufficient memory."),
            Err(e) => error!("Gemma orchestrator start failed: {}", e),
        }
    }

    // Initialize the skills registry and auto-load any .yaml/.json files from
    // .docu/skills/ in the current working directory.
    let skills_registry = skills::registry::SkillsRegistry::new();
    let skills_dir = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join(".docu")
        .join("skills");
    let loaded = skills_registry.load_from_dir(&skills_dir);
    info!("Skills registry initialized ({} skill(s) loaded)", loaded);

    // Initialize tools (with memory attached so browser navigation is auto-indexed).
    let tool_executor = tools::ToolExecutor::new().with_memory(memory_system.clone());
    info!("Tool executor initialized");

    // Initialize the task tracker (persists project/task graph nodes).
    let _task_tracker = indexer::task_tracker::TaskTracker::new(memory_system.clone());
    info!("Task tracker initialized");

    // Start a background UiIndexer that periodically snapshots the foreground window.
    {
        let ui_memory = memory_system.clone();
        let interval_secs = std::env::var("ROOK_UI_POLL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30);
        tokio::spawn(async move {
            let ui_indexer = indexer::ui_indexer::UiIndexer::new(ui_memory);
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
            loop {
                ticker.tick().await;
                if let Err(e) = ui_indexer.capture_and_index().await {
                    tracing::warn!("UiIndexer capture failed: {}", e);
                }
            }
        });
        info!(
            "UI indexer background poller started (every {}s)",
            interval_secs
        );
    }

    // Start file indexer
    let file_indexer = indexer::file_indexer::FileIndexer::new(memory_system.clone());
    file_indexer.start_watching()?;
    info!("File indexer started");

    // Fire up the scheduler tick loop. wakes due tasks every 60s.
    scheduler::r#loop::spawn(memory_system.clone(), llm_client.clone());
    info!("Scheduler loop started");

    // Start the IPC server (Named Pipes)
    let ipc_server =
        ipc::server::IPCServer::new(memory_system, llm_client, skills_registry, tool_executor);

    // Easter egg: set ROOK_VIBES=1 for a slightly more enthusiastic startup.
    if std::env::var("ROOK_VIBES").ok().as_deref() == Some("1") {
        info!("♜  rook online. the board is yours.");
    } else {
        info!("Starting IPC server on pipe: \\\\.\\pipe\\rook");
    }
    tokio::select! {
        result = ipc_server.start() => {
            if let Err(e) = result {
                error!("IPC server error: {}", e);
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Received shutdown signal, exiting gracefully.");
        }
    }

    Ok(())
}

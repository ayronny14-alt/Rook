use anyhow::Result;
use tracing::{error, info, warn};

use super::HandlerCtx;
use crate::ipc::protocol::{IPCResponse, PluginInfo};
use crate::llm::types::LLMConfig;
use crate::plugins::installer::{install_plugin, uninstall_plugin};
use crate::plugins::registry::{Plugin, PluginStatus, PluginType};

pub fn plugin_to_info(p: &Plugin, running: bool) -> PluginInfo {
    PluginInfo {
        id: p.id.clone(),
        name: p.name.clone(),
        description: p.description.clone(),
        plugin_type: p.plugin_type.as_str().to_string(),
        repo_url: p.repo_url.clone(),
        owner: p.owner.clone(),
        repo: p.repo.clone(),
        stars: p.stars,
        entry_point: p.entry_point.clone(),
        install_path: p.install_path.clone(),
        status: p.status.as_str().to_string(),
        enabled: p.enabled,
        running,
    }
}

pub async fn handle_search_plugins(ctx: &HandlerCtx, id: &str, query: &str) -> Result<IPCResponse> {
    let github_token = std::env::var("GITHUB_TOKEN").ok();
    let results = crate::plugins::github::search_github(query, github_token.as_deref())
        .await
        .unwrap_or_default();

    let mut infos: Vec<PluginInfo> = Vec::new();
    for p in &results {
        let existing = ctx.plugin_registry.get_by_repo(&p.repo_url).ok().flatten();
        let plugin = match existing {
            Some(e) => e,
            None => {
                let _ = ctx.plugin_registry.upsert(p);
                p.clone()
            }
        };
        infos.push(plugin_to_info(&plugin, false));
    }
    Ok(IPCResponse::PluginList {
        id: id.to_string(),
        plugins: infos,
    })
}

pub async fn handle_list_plugins(
    ctx: &HandlerCtx,
    id: &str,
    plugin_type: Option<&str>,
) -> Result<IPCResponse> {
    let filter = plugin_type.map(PluginType::from_str);
    let plugins = ctx.plugin_registry.list(filter).unwrap_or_default();
    let running_ids: Vec<String> = ctx
        .mcp_manager
        .lock()
        .map(|m| m.running_ids())
        .unwrap_or_default();
    let infos = plugins
        .iter()
        .map(|p| plugin_to_info(p, running_ids.contains(&p.id)))
        .collect();
    Ok(IPCResponse::PluginList {
        id: id.to_string(),
        plugins: infos,
    })
}

#[allow(clippy::too_many_arguments)]
pub async fn handle_install_plugin(
    ctx: &HandlerCtx,
    id: &str,
    plugin_id: &str,
    name: Option<&str>,
    description: Option<&str>,
    plugin_type: Option<&str>,
    repo_url: Option<&str>,
    owner: Option<&str>,
    repo: Option<&str>,
    stars: Option<i64>,
) -> Result<IPCResponse> {
    // Resolve plugin — look up by id, or upsert from inline data if not yet registered.
    let plugin = match ctx.plugin_registry.get(plugin_id)? {
        Some(p) => p,
        None => {
            // Browse-list plugins haven't been through a SearchPlugins call, so they
            // aren't in the local SQLite registry.  If the caller supplied inline
            // metadata we can upsert them on the fly before installing.
            match (repo_url, name) {
                (Some(rurl), Some(pname)) => {
                    // owner/repo may not be explicit — parse from id (often "owner/repo").
                    let (resolved_owner, resolved_repo) = {
                        let o = owner.unwrap_or("");
                        let r = repo.unwrap_or("");
                        if o.is_empty() || r.is_empty() {
                            let mut parts = plugin_id.splitn(2, '/');
                            let po = parts.next().unwrap_or("").to_string();
                            let pr = parts.next().unwrap_or("").to_string();
                            (
                                if o.is_empty() { po } else { o.to_string() },
                                if r.is_empty() { pr } else { r.to_string() },
                            )
                        } else {
                            (o.to_string(), r.to_string())
                        }
                    };
                    let new_plugin = Plugin {
                        id: plugin_id.to_string(),
                        name: pname.to_string(),
                        description: description.map(str::to_string),
                        plugin_type: PluginType::from_str(plugin_type.unwrap_or("mcp")),
                        repo_url: rurl.to_string(),
                        owner: resolved_owner,
                        repo: resolved_repo,
                        stars: stars.unwrap_or(0),
                        entry_point: None,
                        install_path: None,
                        status: PluginStatus::Available,
                        enabled: false,
                        config_json: None,
                        error_msg: None,
                    };
                    if let Err(e) = ctx.plugin_registry.upsert(&new_plugin) {
                        warn!("Failed to register browse plugin '{}': {}", plugin_id, e);
                    }
                    new_plugin
                }
                _ => {
                    return Ok(IPCResponse::PluginAction {
                        id: id.to_string(),
                        plugin_id: plugin_id.to_string(),
                        action: "install".to_string(),
                        success: false,
                        message: format!("Plugin '{}' not found in registry", plugin_id),
                    });
                }
            }
        }
    };

    let reg = ctx.plugin_registry.clone();
    let p = plugin.clone();
    info!(
        "[handle_install] spawning install task for '{}' owner='{}' repo='{}'",
        p.name, p.owner, p.repo
    );
    tokio::spawn(async move {
        match install_plugin(&p, &reg).await {
            Ok(()) => info!("[handle_install] task finished OK for '{}'", p.id),
            Err(e) => {
                let msg = e.to_string();
                error!("[handle_install] task failed for '{}': {}", p.id, msg);
                let _ = reg.set_status(&p.id, PluginStatus::Error, Some(&msg));
            }
        }
    });
    Ok(IPCResponse::PluginAction {
        id: id.to_string(),
        plugin_id: plugin_id.to_string(),
        action: "install".to_string(),
        success: true,
        message: format!("Installing '{}' in background…", plugin.name),
    })
}

pub async fn handle_set_plugin_enabled(
    ctx: &HandlerCtx,
    id: &str,
    plugin_id: &str,
    enabled: bool,
) -> Result<IPCResponse> {
    match ctx.plugin_registry.get(plugin_id)? {
        None => Ok(IPCResponse::PluginAction {
            id: id.to_string(),
            plugin_id: plugin_id.to_string(),
            action: "set_enabled".to_string(),
            success: false,
            message: "Plugin not found".to_string(),
        }),
        Some(_) => {
            ctx.plugin_registry.set_enabled(plugin_id, enabled)?;
            let verb = if enabled { "enabled" } else { "disabled" };
            Ok(IPCResponse::PluginAction {
                id: id.to_string(),
                plugin_id: plugin_id.to_string(),
                action: "set_enabled".to_string(),
                success: true,
                message: format!("Plugin {}", verb),
            })
        }
    }
}

pub async fn handle_uninstall_plugin(
    ctx: &HandlerCtx,
    id: &str,
    plugin_id: &str,
) -> Result<IPCResponse> {
    match ctx.plugin_registry.get(plugin_id)? {
        None => Ok(IPCResponse::PluginAction {
            id: id.to_string(),
            plugin_id: plugin_id.to_string(),
            action: "uninstall".to_string(),
            success: false,
            message: "Plugin not found".to_string(),
        }),
        Some(plugin) => {
            let reg = ctx.plugin_registry.clone();
            let p = plugin.clone();
            tokio::spawn(async move {
                let _ = uninstall_plugin(&p, &reg).await;
            });
            Ok(IPCResponse::PluginAction {
                id: id.to_string(),
                plugin_id: plugin_id.to_string(),
                action: "uninstall".to_string(),
                success: true,
                message: format!("Uninstalling '{}'…", plugin.name),
            })
        }
    }
}

pub async fn handle_start_mcp(ctx: &HandlerCtx, id: &str, plugin_id: &str) -> Result<IPCResponse> {
    match ctx.plugin_registry.get(plugin_id)? {
        None => Ok(IPCResponse::PluginAction {
            id: id.to_string(),
            plugin_id: plugin_id.to_string(),
            action: "start_mcp".to_string(),
            success: false,
            message: "Plugin not found".to_string(),
        }),
        Some(plugin) => {
            let Some(install_path_str) = plugin.install_path.clone() else {
                return Ok(IPCResponse::PluginAction {
                    id: id.to_string(),
                    plugin_id: plugin_id.to_string(),
                    action: "start_mcp".to_string(),
                    success: false,
                    message: "Plugin is not installed".to_string(),
                });
            };
            let install_path = std::path::PathBuf::from(&install_path_str);

            // If entry_point was never stored (installed before detection fix),
            // detect it now on-the-fly and persist it for next time.
            let entry_point = match plugin.entry_point.clone() {
                Some(ep) => ep,
                None => {
                    use crate::plugins::mcp_runner::{detect_mcp_config, write_mcp_json};
                    match detect_mcp_config(&install_path, plugin_id).await {
                        Some((ep, ref cfg)) => {
                            let _ = ctx.plugin_registry.set_entry_point(plugin_id, &ep);
                            let _ = write_mcp_json(&install_path, plugin_id, cfg).await;
                            ep
                        }
                        None => {
                            return Ok(IPCResponse::PluginAction {
                                id: id.to_string(), plugin_id: plugin_id.to_string(),
                                action: "start_mcp".to_string(), success: false,
                                message: "Could not detect how to start this MCP server. Check the plugin's README.".to_string(),
                            });
                        }
                    }
                }
            };
            let env_json = plugin.config_json.clone();
            let mgr = ctx.mcp_manager.clone();
            let pid = plugin_id.to_string();
            tokio::spawn(async move {
                use crate::plugins::mcp_runner::McpProcess;
                match McpProcess::spawn(&pid, &install_path, &entry_point, env_json.as_deref())
                    .await
                {
                    Ok(proc) => {
                        if let Ok(mut m) = mgr.lock() {
                            m.processes.insert(pid, proc);
                        }
                    }
                    Err(e) => {
                        warn!("Failed to start MCP '{}': {}", pid, e);
                    }
                }
            });
            Ok(IPCResponse::PluginAction {
                id: id.to_string(),
                plugin_id: plugin_id.to_string(),
                action: "start_mcp".to_string(),
                success: true,
                message: "MCP server starting…".to_string(),
            })
        }
    }
}

pub async fn handle_stop_mcp(ctx: &HandlerCtx, id: &str, plugin_id: &str) -> Result<IPCResponse> {
    let mgr = ctx.mcp_manager.clone();
    let pid = plugin_id.to_string();
    tokio::spawn(async move {
        let proc = mgr.lock().ok().and_then(|mut m| m.processes.remove(&pid));
        if let Some(p) = proc {
            let _ = p.kill().await;
        }
    });
    Ok(IPCResponse::PluginAction {
        id: id.to_string(),
        plugin_id: plugin_id.to_string(),
        action: "stop_mcp".to_string(),
        success: true,
        message: "MCP server stopping…".to_string(),
    })
}

pub async fn handle_launch_gemma(ctx: &HandlerCtx, id: &str, model: &str) -> Result<IPCResponse> {
    info!("LaunchGemma request for model '{}'", model);
    match ctx.gemma.try_start().await {
        Ok(true) => Ok(IPCResponse::GemmaLaunched {
            id: id.to_string(),
            success: true,
            message: format!("Gemma is up and responding. Model in use: {}", model),
        }),
        Ok(false) => Ok(IPCResponse::GemmaLaunched {
            id: id.to_string(),
            success: false,
            message: "Not enough free RAM to start Gemma. Free at least 6 GiB and try again."
                .to_string(),
        }),
        Err(e) => Ok(IPCResponse::GemmaLaunched {
            id: id.to_string(),
            success: false,
            message: format!("Failed to launch Gemma: {}", e),
        }),
    }
}

pub async fn handle_update_config(
    ctx: &HandlerCtx,
    id: &str,
    base_url: &str,
    api_key: &str,
    model: &str,
) -> Result<IPCResponse> {
    info!("UpdateConfig: base_url={} model={}", base_url, model);
    let mut cfg: LLMConfig = ctx.llm.base_config().clone();
    if !base_url.trim().is_empty() {
        cfg.base_url = base_url.to_string();
    }
    if !api_key.trim().is_empty() {
        cfg.api_key = api_key.to_string();
        // if embedding key was never set separately, keep it in sync
        if cfg.embedding_api_key.trim().is_empty() {
            cfg.embedding_api_key = api_key.to_string();
        }
    }
    if !model.trim().is_empty() {
        cfg.model = model.to_string();
    }
    if let Ok(mut guard) = ctx.config_override.lock() {
        *guard = Some(cfg.clone());
    }
    // survive backend restarts — write to %LOCALAPPDATA%\Rook\config.json
    if let Err(e) = persist_config(&cfg) {
        warn!("failed to persist config: {}", e);
    }
    Ok(IPCResponse::ConfigUpdated {
        id: id.to_string(),
        success: true,
    })
}

fn persist_config(cfg: &LLMConfig) -> anyhow::Result<()> {
    let dir = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("Rook");
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_string(cfg)?;
    std::fs::write(dir.join("config.json"), json)?;
    Ok(())
}

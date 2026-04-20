/// Plugin installer - clones a GitHub repo to the local plugins directory,
/// detects its entry point, and registers it in the PluginRegistry.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tracing::{error, info, warn};

use super::mcp_runner::{detect_mcp_config, write_mcp_json};
use super::registry::{Plugin, PluginRegistry, PluginStatus};

/// Returns the root directory where plugins are installed.
/// Default: `%LOCALAPPDATA%\Rook\plugins\` (Windows) or `~/.local/share/rook/plugins`
pub fn plugins_root() -> PathBuf {
    if let Some(local) = dirs::data_local_dir() {
        local.join("Rook").join("plugins")
    } else {
        PathBuf::from(".docu").join("plugins")
    }
}

// Clone or update a plugin repo and register/update it in the database.
pub async fn install_plugin(plugin: &Plugin, registry: &PluginRegistry) -> Result<()> {
    info!(
        "[install] start id={} owner={} repo={}",
        plugin.id, plugin.owner, plugin.repo
    );

    if plugin.owner.is_empty() || plugin.repo.is_empty() {
        let msg = format!(
            "owner/repo missing for plugin '{}' - cannot build clone URL",
            plugin.id
        );
        error!("[install] {}", msg);
        registry.set_status(&plugin.id, PluginStatus::Error, Some(&msg))?;
        return Err(anyhow::anyhow!(msg));
    }

    registry.set_status(&plugin.id, PluginStatus::Installing, None)?;

    let install_dir = plugins_root().join(&plugin.owner).join(&plugin.repo);
    info!("[install] install_dir = {:?}", install_dir);

    tokio::fs::create_dir_all(&install_dir)
        .await
        .context("create plugins dir")?;

    // 1. git clone / pull
    let clone_url = format!("https://github.com/{}/{}.git", plugin.owner, plugin.repo);
    if install_dir.join(".git").exists() {
        info!("[install] pulling {}", clone_url);
        match run_command("git", &["pull", "--ff-only"], &install_dir).await {
            Ok(_) => info!("[install] git pull OK"),
            Err(e) => {
                error!("[install] git pull failed: {}", e);
                return Err(e);
            }
        }
    } else {
        info!("[install] cloning {}", clone_url);
        match run_command(
            "git",
            &["clone", "--depth=1", &clone_url, "."],
            &install_dir,
        )
        .await
        {
            Ok(_) => info!("[install] git clone OK"),
            Err(e) => {
                error!("[install] git clone failed: {}", e);
                let _ = registry.set_status(&plugin.id, PluginStatus::Error, Some(&e.to_string()));
                return Err(e);
            }
        }
    }

    // 1b. Manifest verification
    // If the repo ships a `rook.json` manifest, validate it. If no manifest
    // exists we warn but allow the install - most community plugins predate the
    // spec. A missing manifest is a yellow flag, not a hard block.
    let manifest_path = install_dir.join("rook.json");
    if manifest_path.exists() {
        match tokio::fs::read_to_string(&manifest_path).await {
            Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(m) if m.get("name").is_some() && m.get("version").is_some() => {
                    info!(
                        "[install] manifest OK: name={:?} version={:?}",
                        m["name"], m["version"]
                    );
                }
                Ok(_) => {
                    let msg = "Plugin manifest (rook.json) is missing required 'name' or 'version' fields - install aborted for safety.";
                    warn!("[install] {}", msg);
                    let _ = registry.set_status(&plugin.id, PluginStatus::Error, Some(msg));
                    return Err(anyhow::anyhow!(msg));
                }
                Err(e) => {
                    let msg = format!(
                        "Plugin manifest (rook.json) is not valid JSON: {} - install aborted.",
                        e
                    );
                    warn!("[install] {}", msg);
                    let _ = registry.set_status(&plugin.id, PluginStatus::Error, Some(&msg));
                    return Err(anyhow::anyhow!(msg));
                }
            },
            Err(e) => warn!("[install] could not read rook.json (non-fatal): {}", e),
        }
    } else {
        warn!(
            "[install] no rook.json manifest found for plugin '{}' - \
             installing unverified code from {}/{}. Review the repo before enabling.",
            plugin.id, plugin.owner, plugin.repo
        );
    }

    // 2. Detect entry point + write mcp.json
    // npx / uvx handle dependencies on first invocation so we skip npm/pip install.
    let mcp = detect_mcp_config(&install_dir, &plugin.id).await;
    let entry_point = mcp.as_ref().map(|(ep, _)| ep.as_str());
    info!("[install] entry_point = {:?}", entry_point);

    // Write mcp.json alongside the cloned repo so users can inspect / copy it.
    if let Some((_, ref cfg)) = mcp {
        if let Err(e) = write_mcp_json(&install_dir, &plugin.id, cfg).await {
            warn!("[install] mcp.json write failed (non-fatal): {}", e);
        }
    }

    // 3. Register
    let path_str = install_dir.to_string_lossy().into_owned();
    registry.set_installed(&plugin.id, &path_str, entry_point)?;
    info!("[install] done id={} path={}", plugin.id, path_str);

    Ok(())
}

/// Uninstall a plugin: remove its directory and mark it available in the DB.
pub async fn uninstall_plugin(plugin: &Plugin, registry: &PluginRegistry) -> Result<()> {
    if let Some(ref path) = plugin.install_path {
        let p = Path::new(path);
        if p.exists() {
            tokio::fs::remove_dir_all(p)
                .await
                .context("remove plugin directory")?;
        }
    }
    registry.set_status(&plugin.id, PluginStatus::Available, None)?;
    registry.set_enabled(&plugin.id, false)?;
    info!("Plugin '{}' uninstalled.", plugin.name);
    Ok(())
}

// Helpers

pub(crate) async fn run_cmd_test(program: &str, args: &[&str], cwd: &Path) -> Result<String> {
    run_command(program, args, cwd).await
}

async fn run_command(program: &str, args: &[&str], cwd: &Path) -> Result<String> {
    let mut cmd = tokio::process::Command::new(program);
    let child = crate::os::hide_tokio(&mut cmd)
        .args(args)
        .current_dir(cwd)
        .output();

    let output = tokio::time::timeout(std::time::Duration::from_secs(180), child)
        .await
        .with_context(|| format!("{} timed out after 180s", program))?
        .with_context(|| format!("run {} {:?}", program, args))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&output.stderr).into_owned();
        warn!("{} failed: {}", program, err.trim());
        Err(anyhow::anyhow!("{} failed: {}", program, err.trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::storage::MemoryStorage;
    use crate::plugins::registry::{Plugin, PluginRegistry, PluginStatus, PluginType};

    fn make_plugin(owner: &str, repo: &str) -> Plugin {
        Plugin {
            id: format!("{}/{}", owner, repo),
            name: repo.to_string(),
            description: None,
            plugin_type: PluginType::Mcp,
            repo_url: format!("https://github.com/{}/{}", owner, repo),
            owner: owner.to_string(),
            repo: repo.to_string(),
            stars: 0,
            entry_point: None,
            install_path: None,
            status: PluginStatus::Available,
            enabled: false,
            config_json: None,
            error_msg: None,
        }
    }

    #[test]
    fn upsert_and_status_roundtrip() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let registry = PluginRegistry::new(storage);
        let p = make_plugin("octocat", "Hello-World");

        registry.upsert(&p).unwrap();
        let got = registry.get("octocat/Hello-World").unwrap().unwrap();
        assert_eq!(got.owner, "octocat");
        assert_eq!(got.repo, "Hello-World");
        assert_eq!(got.status, PluginStatus::Available);

        registry
            .set_status("octocat/Hello-World", PluginStatus::Installing, None)
            .unwrap();
        let got2 = registry.get("octocat/Hello-World").unwrap().unwrap();
        assert_eq!(got2.status, PluginStatus::Installing);

        registry
            .set_installed("octocat/Hello-World", "/tmp/test", None)
            .unwrap();
        let got3 = registry.get("octocat/Hello-World").unwrap().unwrap();
        assert_eq!(got3.status, PluginStatus::Installed);
        println!("DB roundtrip OK: {:?}", got3.status);
    }

    #[tokio::test]
    async fn git_clone_small_repo() {
        let tmp = std::env::temp_dir().join("rook-test-clone");
        if tmp.exists() {
            tokio::fs::remove_dir_all(&tmp).await.unwrap();
        }
        tokio::fs::create_dir_all(&tmp).await.unwrap();

        let result = run_cmd_test(
            "git",
            &[
                "clone",
                "--depth=1",
                "https://github.com/octocat/Hello-World.git",
                ".",
            ],
            &tmp,
        )
        .await;

        eprintln!("git clone result: {:?}", result);
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        assert!(result.is_ok(), "git clone failed: {:?}", result.err());
    }

    #[tokio::test]
    async fn full_install_flow_small_repo() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let registry = PluginRegistry::new(storage);

        // Use a tiny repo with no build steps
        let p = make_plugin("octocat", "Hello-World");

        // Override install dir to temp so we don't pollute AppData
        let tmp_root = std::env::temp_dir().join("rook-full-install-test");
        if tmp_root.exists() {
            tokio::fs::remove_dir_all(&tmp_root).await.unwrap();
        }

        registry.upsert(&p).unwrap();

        // Patch install dir inline by running the steps manually
        let install_dir = tmp_root.join("octocat").join("Hello-World");
        tokio::fs::create_dir_all(&install_dir).await.unwrap();

        registry
            .set_status(&p.id, PluginStatus::Installing, None)
            .unwrap();

        let clone_url = format!("https://github.com/{}/{}.git", p.owner, p.repo);
        let clone_result = run_cmd_test(
            "git",
            &["clone", "--depth=1", &clone_url, "."],
            &install_dir,
        )
        .await;
        eprintln!("clone: {:?}", clone_result);

        let path_str = install_dir.to_string_lossy().into_owned();
        match clone_result {
            Ok(_) => {
                registry.set_installed(&p.id, &path_str, None).unwrap();
            }
            Err(e) => {
                registry
                    .set_status(&p.id, PluginStatus::Error, Some(&e.to_string()))
                    .unwrap();
            }
        }

        let final_plugin = registry.get(&p.id).unwrap().unwrap();
        eprintln!(
            "Final status: {:?} | install_path: {:?}",
            final_plugin.status, final_plugin.install_path
        );

        let _ = tokio::fs::remove_dir_all(&tmp_root).await;

        assert_eq!(
            final_plugin.status,
            PluginStatus::Installed,
            "Plugin should be installed"
        );
        assert!(final_plugin.install_path.is_some());
    }
}

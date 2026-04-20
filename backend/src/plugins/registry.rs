use anyhow::Result;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::memory::storage::MemoryStorage;

// Types 

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PluginType {
    Skill,
    Connector,
    Mcp,
}

impl PluginType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Skill => "skill",
            Self::Connector => "connector",
            Self::Mcp => "mcp",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "connector" => Self::Connector,
            "mcp" => Self::Mcp,
            _ => Self::Skill,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PluginStatus {
    Available,
    Installing,
    Installed,
    Error,
}

impl PluginStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Installing => "installing",
            Self::Installed => "installed",
            Self::Error => "error",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "installing" => Self::Installing,
            "installed" => Self::Installed,
            "error" => Self::Error,
            _ => Self::Available,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plugin {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub plugin_type: PluginType,
    pub repo_url: String,
    pub owner: String,
    pub repo: String,
    pub stars: i64,
    pub entry_point: Option<String>,
    pub install_path: Option<String>,
    pub status: PluginStatus,
    pub enabled: bool,
    pub config_json: Option<String>,
    pub error_msg: Option<String>,
}

// Registry 

#[derive(Clone)]
pub struct PluginRegistry {
    memory: MemoryStorage,
}

impl PluginRegistry {
    pub fn new(memory: MemoryStorage) -> Self {
        Self { memory }
    }

    /// Upsert a plugin record (insert or update by repo_url).
    pub fn upsert(&self, p: &Plugin) -> Result<()> {
        let conn = self
            .memory
            .get_connection()
            .map_err(|e| anyhow::anyhow!(e))?;
        let now = now_secs();
        conn.execute(
            "INSERT INTO plugins (id, name, description, plugin_type, repo_url, owner, repo,
             stars, entry_point, install_path, status, enabled, config_json, error_msg,
             created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
             ON CONFLICT(id) DO UPDATE SET
               name=excluded.name, description=excluded.description,
               plugin_type=excluded.plugin_type, stars=excluded.stars,
               entry_point=excluded.entry_point, install_path=excluded.install_path,
               status=excluded.status, enabled=excluded.enabled,
               config_json=excluded.config_json, error_msg=excluded.error_msg,
               updated_at=excluded.updated_at",
            rusqlite::params![
                &p.id,
                &p.name,
                &p.description,
                p.plugin_type.as_str(),
                &p.repo_url,
                &p.owner,
                &p.repo,
                p.stars,
                &p.entry_point,
                &p.install_path,
                p.status.as_str(),
                p.enabled as i64,
                &p.config_json,
                &p.error_msg,
                now,
                now
            ],
        )?;
        Ok(())
    }

    /// Update status + optional error message for a plugin.
    pub fn set_status(
        &self,
        id: &str,
        status: PluginStatus,
        error_msg: Option<&str>,
    ) -> Result<()> {
        let conn = self
            .memory
            .get_connection()
            .map_err(|e| anyhow::anyhow!(e))?;
        conn.execute(
            "UPDATE plugins SET status=?1, error_msg=?2, updated_at=?3 WHERE id=?4",
            rusqlite::params![status.as_str(), error_msg, now_secs(), id],
        )?;
        Ok(())
    }

    /// Set install_path and entry_point after a successful install.
    pub fn set_installed(
        &self,
        id: &str,
        install_path: &str,
        entry_point: Option<&str>,
    ) -> Result<()> {
        let conn = self
            .memory
            .get_connection()
            .map_err(|e| anyhow::anyhow!(e))?;
        conn.execute(
            "UPDATE plugins SET status='installed', install_path=?1, entry_point=?2,
             installed_at=?3, updated_at=?3 WHERE id=?4",
            rusqlite::params![install_path, entry_point, now_secs(), id],
        )?;
        Ok(())
    }

    /// Update the entry_point for a plugin (e.g. detected on-the-fly during start_mcp).
    pub fn set_entry_point(&self, id: &str, entry_point: &str) -> Result<()> {
        let conn = self
            .memory
            .get_connection()
            .map_err(|e| anyhow::anyhow!(e))?;
        conn.execute(
            "UPDATE plugins SET entry_point=?1, updated_at=?2 WHERE id=?3",
            rusqlite::params![entry_point, now_secs(), id],
        )?;
        Ok(())
    }

    /// Enable or disable a plugin.
    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        let conn = self
            .memory
            .get_connection()
            .map_err(|e| anyhow::anyhow!(e))?;
        conn.execute(
            "UPDATE plugins SET enabled=?1, updated_at=?2 WHERE id=?3",
            rusqlite::params![enabled as i64, now_secs(), id],
        )?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Option<Plugin>> {
        let conn = self
            .memory
            .get_connection()
            .map_err(|e| anyhow::anyhow!(e))?;
        let mut stmt = conn.prepare(
            "SELECT id,name,description,plugin_type,repo_url,owner,repo,stars,
             entry_point,install_path,status,enabled,config_json,error_msg
             FROM plugins WHERE id=?1",
        )?;
        let mut rows = stmt.query([id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_plugin(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn get_by_repo(&self, repo_url: &str) -> Result<Option<Plugin>> {
        let conn = self
            .memory
            .get_connection()
            .map_err(|e| anyhow::anyhow!(e))?;
        let mut stmt = conn.prepare(
            "SELECT id,name,description,plugin_type,repo_url,owner,repo,stars,
             entry_point,install_path,status,enabled,config_json,error_msg
             FROM plugins WHERE repo_url=?1 LIMIT 1",
        )?;
        let mut rows = stmt.query([repo_url])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_plugin(row)?))
        } else {
            Ok(None)
        }
    }

    /// List all plugins, optionally filtered by type.
    pub fn list(&self, filter_type: Option<PluginType>) -> Result<Vec<Plugin>> {
        let conn = self
            .memory
            .get_connection()
            .map_err(|e| anyhow::anyhow!(e))?;
        let sql = match &filter_type {
            Some(_) => {
                "SELECT id,name,description,plugin_type,repo_url,owner,repo,stars,
                 entry_point,install_path,status,enabled,config_json,error_msg
                 FROM plugins WHERE plugin_type=?1 ORDER BY stars DESC"
            }
            None => {
                "SELECT id,name,description,plugin_type,repo_url,owner,repo,stars,
                 entry_point,install_path,status,enabled,config_json,error_msg
                 FROM plugins ORDER BY stars DESC, status ASC"
            }
        };

        let type_str = filter_type.as_ref().map(|t| t.as_str()).unwrap_or("");
        let mut stmt = conn.prepare(sql)?;
        let rows = if filter_type.is_some() {
            stmt.query([type_str])?
        } else {
            stmt.query([])?
        };

        collect_plugins(rows)
    }

    /// List only installed + enabled plugins (used to load active MCPs on startup).
    pub fn list_active_mcps(&self) -> Result<Vec<Plugin>> {
        let conn = self
            .memory
            .get_connection()
            .map_err(|e| anyhow::anyhow!(e))?;
        let mut stmt = conn.prepare(
            "SELECT id,name,description,plugin_type,repo_url,owner,repo,stars,
             entry_point,install_path,status,enabled,config_json,error_msg
             FROM plugins WHERE plugin_type='mcp' AND status='installed' AND enabled=1",
        )?;
        let rows = stmt.query([])?;
        collect_plugins(rows)
    }

    /// Delete a plugin record.
    pub fn remove(&self, id: &str) -> Result<()> {
        let conn = self
            .memory
            .get_connection()
            .map_err(|e| anyhow::anyhow!(e))?;
        conn.execute("DELETE FROM plugins WHERE id=?1", [id])?;
        Ok(())
    }

    /// Search installed & enabled plugins whose name/description match a query.
    pub fn search_active(&self, query: &str) -> Result<Vec<Plugin>> {
        let conn = self
            .memory
            .get_connection()
            .map_err(|e| anyhow::anyhow!(e))?;
        let pattern = format!("%{}%", query.to_lowercase());
        let mut stmt = conn.prepare(
            "SELECT id,name,description,plugin_type,repo_url,owner,repo,stars,
             entry_point,install_path,status,enabled,config_json,error_msg
             FROM plugins
             WHERE enabled=1 AND status='installed'
               AND (lower(name) LIKE ?1 OR lower(description) LIKE ?1)
             ORDER BY stars DESC LIMIT 10",
        )?;
        let rows = stmt.query([&pattern])?;
        collect_plugins(rows)
    }
}

// Helpers 

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn row_to_plugin(row: &rusqlite::Row<'_>) -> rusqlite::Result<Plugin> {
    Ok(Plugin {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        plugin_type: PluginType::from_str(&row.get::<_, String>(3)?),
        repo_url: row.get(4)?,
        owner: row.get(5)?,
        repo: row.get(6)?,
        stars: row.get(7)?,
        entry_point: row.get(8)?,
        install_path: row.get(9)?,
        status: PluginStatus::from_str(&row.get::<_, String>(10)?),
        enabled: row.get::<_, i64>(11)? != 0,
        config_json: row.get(12)?,
        error_msg: row.get(13)?,
    })
}

fn collect_plugins(mut rows: rusqlite::Rows<'_>) -> Result<Vec<Plugin>> {
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(row_to_plugin(row)?);
    }
    Ok(out)
}

/// Build a new Plugin from GitHub search result data (not yet installed).
pub fn plugin_from_github(
    name: &str,
    description: Option<String>,
    plugin_type: PluginType,
    html_url: &str,
    owner: &str,
    repo: &str,
    stars: i64,
) -> Plugin {
    Plugin {
        id: Uuid::new_v4().to_string(),
        name: name.to_string(),
        description,
        plugin_type,
        repo_url: html_url.to_string(),
        owner: owner.to_string(),
        repo: repo.to_string(),
        stars,
        entry_point: None,
        install_path: None,
        status: PluginStatus::Available,
        enabled: false,
        config_json: None,
        error_msg: None,
    }
}

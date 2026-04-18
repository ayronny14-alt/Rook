// SQLite CRUD for scheduled_tasks. schema lives in memory/storage.rs.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use crate::memory::storage::MemoryStorage;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Proposed, // ai suggested, awaiting user approval
    Active,
    Paused,
    Archived, // one-shot completed or user killed it
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Archived => "archived",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s {
            "proposed" => Self::Proposed,
            "active" => Self::Active,
            "paused" => Self::Paused,
            "archived" => Self::Archived,
            _ => Self::Active,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskSource {
    User,
    Ai,
}

impl TaskSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Ai => "ai",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s {
            "ai" => Self::Ai,
            _ => Self::User,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTask {
    pub id: String,
    pub name: String,
    pub cadence: String,
    pub prompt: String,
    pub output_channel: String, // notification|silent|telegram|email
    pub status: TaskStatus,
    pub created_by: TaskSource,
    pub last_run_at: Option<i64>,
    pub next_run_at: i64,
    pub run_count: i64,
    pub created_at: i64,
    pub why: Option<String>, // AI's reasoning when proposing
}

pub struct SchedulerStore {
    memory: MemoryStorage,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

impl SchedulerStore {
    pub fn new(memory: MemoryStorage) -> Self {
        Self { memory }
    }

    pub fn insert(&self, task: &ScheduledTask) -> Result<()> {
        let conn = self.memory.get_connection().map_err(anyhow::Error::msg)?;
        conn.execute(
            "INSERT INTO scheduled_tasks
             (id, name, cadence, prompt, output_channel, status, created_by,
              last_run_at, next_run_at, run_count, created_at, why)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                task.id,
                task.name,
                task.cadence,
                task.prompt,
                task.output_channel,
                task.status.as_str(),
                task.created_by.as_str(),
                task.last_run_at,
                task.next_run_at,
                task.run_count,
                task.created_at,
                task.why,
            ],
        )
        .context("insert scheduled_task")?;
        Ok(())
    }

    pub fn list(&self, include_archived: bool) -> Result<Vec<ScheduledTask>> {
        let conn = self.memory.get_connection().map_err(anyhow::Error::msg)?;
        let sql = if include_archived {
            "SELECT id, name, cadence, prompt, output_channel, status, created_by,
             last_run_at, next_run_at, run_count, created_at, why
             FROM scheduled_tasks ORDER BY next_run_at ASC"
        } else {
            "SELECT id, name, cadence, prompt, output_channel, status, created_by,
             last_run_at, next_run_at, run_count, created_at, why
             FROM scheduled_tasks WHERE status != 'archived' ORDER BY next_run_at ASC"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], row_to_task)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn due(&self) -> Result<Vec<ScheduledTask>> {
        let conn = self.memory.get_connection().map_err(anyhow::Error::msg)?;
        let mut stmt = conn.prepare(
            "SELECT id, name, cadence, prompt, output_channel, status, created_by,
             last_run_at, next_run_at, run_count, created_at, why
             FROM scheduled_tasks
             WHERE status = 'active' AND next_run_at <= ?1
             ORDER BY next_run_at ASC",
        )?;
        let rows = stmt.query_map([now_secs()], row_to_task)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn update_after_fire(&self, id: &str, next_run_at: Option<i64>) -> Result<()> {
        let conn = self.memory.get_connection().map_err(anyhow::Error::msg)?;
        if let Some(next) = next_run_at {
            conn.execute(
                "UPDATE scheduled_tasks
                 SET last_run_at = ?1, next_run_at = ?2, run_count = run_count + 1
                 WHERE id = ?3",
                rusqlite::params![now_secs(), next, id],
            )?;
        } else {
            // one-shot: archive
            conn.execute(
                "UPDATE scheduled_tasks
                 SET last_run_at = ?1, run_count = run_count + 1, status = 'archived'
                 WHERE id = ?2",
                rusqlite::params![now_secs(), id],
            )?;
        }
        Ok(())
    }

    pub fn set_status(&self, id: &str, status: TaskStatus) -> Result<()> {
        let conn = self.memory.get_connection().map_err(anyhow::Error::msg)?;
        conn.execute(
            "UPDATE scheduled_tasks SET status = ?1 WHERE id = ?2",
            rusqlite::params![status.as_str(), id],
        )?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Option<ScheduledTask>> {
        let conn = self.memory.get_connection().map_err(anyhow::Error::msg)?;
        let mut stmt = conn.prepare(
            "SELECT id, name, cadence, prompt, output_channel, status, created_by,
             last_run_at, next_run_at, run_count, created_at, why
             FROM scheduled_tasks WHERE id = ?1",
        )?;
        let mut rows = stmt.query([id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_task(row)?))
        } else {
            Ok(None)
        }
    }
}

pub fn new_task(
    name: String,
    cadence: String,
    prompt: String,
    output_channel: String,
    created_by: TaskSource,
    next_run_at: i64,
    why: Option<String>,
) -> ScheduledTask {
    let status = match created_by {
        TaskSource::Ai
            if std::env::var("ROOK_AI_SCHEDULES_AUTO_APPROVE")
                .ok()
                .as_deref()
                != Some("1") =>
        {
            TaskStatus::Proposed
        }
        _ => TaskStatus::Active,
    };
    ScheduledTask {
        id: Uuid::new_v4().to_string(),
        name,
        cadence,
        prompt,
        output_channel,
        status,
        created_by,
        last_run_at: None,
        next_run_at,
        run_count: 0,
        created_at: now_secs(),
        why,
    }
}

fn row_to_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<ScheduledTask> {
    let status: String = row.get(5)?;
    let created_by: String = row.get(6)?;
    Ok(ScheduledTask {
        id: row.get(0)?,
        name: row.get(1)?,
        cadence: row.get(2)?,
        prompt: row.get(3)?,
        output_channel: row.get(4)?,
        status: TaskStatus::from_str(&status),
        created_by: TaskSource::from_str(&created_by),
        last_run_at: row.get(7)?,
        next_run_at: row.get(8)?,
        run_count: row.get(9)?,
        created_at: row.get(10)?,
        why: row.get(11)?,
    })
}

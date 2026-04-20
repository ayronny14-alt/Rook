// renderer-facing scheduler operations. /schedule modal writes through these.

use anyhow::Result;
use chrono::Local;

use super::HandlerCtx;
use crate::ipc::protocol::IPCResponse;
use crate::scheduler::cadence;
use crate::scheduler::store::{new_task, SchedulerStore};
use crate::scheduler::{TaskSource, TaskStatus};

pub async fn handle_list_scheduled_tasks(ctx: &HandlerCtx, id: &str) -> Result<IPCResponse> {
    let store = SchedulerStore::new(ctx.memory.clone());
    let rows = store.list(false).unwrap_or_default();
    let tasks = rows
        .into_iter()
        .map(|t| serde_json::to_value(t).unwrap_or_default())
        .collect();
    Ok(IPCResponse::ScheduledTaskList {
        id: id.to_string(),
        tasks,
    })
}

pub async fn handle_create_scheduled_task(
    ctx: &HandlerCtx,
    id: &str,
    name: &str,
    cadence_spec: &str,
    prompt: &str,
    output_channel: Option<&str>,
) -> Result<IPCResponse> {
    if name.trim().is_empty() || cadence_spec.trim().is_empty() || prompt.trim().is_empty() {
        return Ok(IPCResponse::ScheduledTaskAction {
            id: id.to_string(),
            task_id: String::new(),
            action: "create".to_string(),
            success: false,
            message: "name, cadence, and prompt are required".to_string(),
        });
    }
    let (next, _kind) = match cadence::parse(cadence_spec, Local::now()) {
        Ok(v) => v,
        Err(e) => {
            return Ok(IPCResponse::ScheduledTaskAction {
                id: id.to_string(),
                task_id: String::new(),
                action: "create".to_string(),
                success: false,
                message: format!("bad cadence: {}", e),
            })
        }
    };
    let task = new_task(
        name.to_string(),
        cadence_spec.to_string(),
        prompt.to_string(),
        output_channel.unwrap_or("notification").to_string(),
        TaskSource::User,
        next,
        None,
    );
    let store = SchedulerStore::new(ctx.memory.clone());
    match store.insert(&task) {
        Ok(_) => Ok(IPCResponse::ScheduledTaskAction {
            id: id.to_string(),
            task_id: task.id.clone(),
            action: "create".to_string(),
            success: true,
            message: format!(
                "scheduled '{}' - next run at {}",
                task.name, task.next_run_at
            ),
        }),
        Err(e) => Ok(IPCResponse::ScheduledTaskAction {
            id: id.to_string(),
            task_id: String::new(),
            action: "create".to_string(),
            success: false,
            message: format!("insert failed: {}", e),
        }),
    }
}

pub async fn handle_set_status(
    ctx: &HandlerCtx,
    id: &str,
    task_id: &str,
    status: TaskStatus,
    action: &str,
) -> Result<IPCResponse> {
    let store = SchedulerStore::new(ctx.memory.clone());
    match store.set_status(task_id, status) {
        Ok(_) => Ok(IPCResponse::ScheduledTaskAction {
            id: id.to_string(),
            task_id: task_id.to_string(),
            action: action.to_string(),
            success: true,
            message: format!("{} ok", action),
        }),
        Err(e) => Ok(IPCResponse::ScheduledTaskAction {
            id: id.to_string(),
            task_id: task_id.to_string(),
            action: action.to_string(),
            success: false,
            message: e.to_string(),
        }),
    }
}

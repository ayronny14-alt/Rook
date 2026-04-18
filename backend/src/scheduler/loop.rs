// tokio tick loop. every 60s: scan scheduled_tasks for due rows, fire each
// in its own task (ReAct-style run with the task prompt), then reschedule
// or archive based on cadence kind.

use chrono::Local;
use std::time::Duration;
use tracing::{info, warn};

use crate::llm::client::LLMClient;
use crate::llm::types::Message;
use crate::memory::storage::MemoryStorage;
use crate::scheduler::cadence;
use crate::scheduler::channels;
use crate::scheduler::store::SchedulerStore;

pub fn spawn(memory: MemoryStorage, llm: LLMClient) {
    tokio::spawn(async move {
        // small initial delay so startup logs settle first
        tokio::time::sleep(Duration::from_secs(10)).await;
        let store = SchedulerStore::new(memory.clone());
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        loop {
            ticker.tick().await;
            match store.due() {
                Ok(tasks) if !tasks.is_empty() => {
                    info!("scheduler: {} task(s) due", tasks.len());
                    for task in tasks {
                        let store = SchedulerStore::new(memory.clone());
                        let llm = llm.clone();
                        tokio::spawn(async move { fire(store, llm, task).await });
                    }
                }
                Ok(_) => {}
                Err(e) => warn!("scheduler query failed: {}", e),
            }
        }
    });
}

async fn fire(store: SchedulerStore, llm: LLMClient, task: crate::scheduler::store::ScheduledTask) {
    info!("scheduler firing '{}' ({})", task.name, task.id);

    // run the prompt against the cheapest model to keep scheduled tasks light.
    // users who want the primary model for scheduled work can opt in via
    // ROOK_SCHEDULED_USE_PRIMARY=1 in the env.
    let use_primary = std::env::var("ROOK_SCHEDULED_USE_PRIMARY").ok().as_deref() == Some("1");
    let model = if use_primary {
        llm.base_config().model.clone()
    } else {
        llm.cheapest_model().await
    };

    let preamble = format!(
        "You are Rook running a scheduled task named '{}'. Reply in a concise way that can \
         be shown as a desktop notification (one or two sentences).",
        task.name
    );
    let messages = vec![
        Message::text("system", preamble),
        Message::text("user", task.prompt.clone()),
    ];

    let result = match llm.chat_with_model_override(messages, &model, 512).await {
        Ok(r) => r
            .choices
            .first()
            .and_then(|c| c.message.text_content())
            .unwrap_or_default(),
        Err(e) => {
            warn!("scheduled task '{}' LLM call failed: {}", task.name, e);
            format!("(task '{}' failed: {})", task.name, e)
        }
    };

    // deliver
    channels::dispatch(&task.output_channel, &task.name, &result);

    // reschedule or archive
    match cadence::next_after(&task.cadence, Local::now()) {
        Ok(next) => {
            if let Err(e) = store.update_after_fire(&task.id, Some(next)) {
                warn!("scheduler update failed for {}: {}", task.id, e);
            }
        }
        Err(_) => {
            // one-shot
            if let Err(e) = store.update_after_fire(&task.id, None) {
                warn!("scheduler archive failed for {}: {}", task.id, e);
            }
        }
    }
}

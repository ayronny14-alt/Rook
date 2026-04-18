// task scheduler. three entry points: /schedule slash command, AI proposal tool,
// and AI direct-create (gated). one tokio ticker wakes due tasks every 60s.

pub mod cadence;
pub mod channels;
pub mod r#loop;
pub mod store;

pub use store::{TaskSource, TaskStatus};

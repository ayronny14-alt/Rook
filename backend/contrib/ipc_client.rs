// Moved from src/ipc/client.rs into contrib. Kept for reference.
use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tracing::{info, debug};

#[allow(dead_code)]
use crate::ipc::protocol::{IPCRequest, IPCResponse};

const PIPE_NAME: &str = r"\\.\pipe\docubot";

pub struct IPCClient {
    stream: std::sync::Arc<Mutex<tokio::net::windows::named_pipe::NamedPipeClient>>,
}

impl IPCClient {
    pub async fn connect() -> Result<Self> {
        info!("Connecting to named pipe: {}", PIPE_NAME);
        let stream = tokio::net::windows::named_pipe::ClientOptions::new()
            .open(PIPE_NAME)?;
        info!("Connected to IPC server");
        Ok(Self {
            stream: std::sync::Arc::new(Mutex::new(stream)),
        })
    }

    // ... other methods retained but not exported here ...
}

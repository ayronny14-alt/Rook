use anyhow::Result;

use super::HandlerCtx;
use crate::ipc::protocol::IPCResponse;

pub async fn handle_git_status(
    ctx: &HandlerCtx,
    id: &str,
    path: Option<&str>,
) -> Result<IPCResponse> {
    let output = ctx.tools.git_status(path).await?;
    Ok(IPCResponse::GitResult {
        id: id.to_string(),
        output,
    })
}

pub async fn handle_git_log(ctx: &HandlerCtx, id: &str, n: usize) -> Result<IPCResponse> {
    let output = ctx.tools.git_log(n).await?;
    Ok(IPCResponse::GitResult {
        id: id.to_string(),
        output,
    })
}

pub async fn handle_git_diff(
    ctx: &HandlerCtx,
    id: &str,
    file_path: Option<&str>,
) -> Result<IPCResponse> {
    let output = ctx.tools.git_diff(file_path).await?;
    Ok(IPCResponse::GitResult {
        id: id.to_string(),
        output,
    })
}

pub async fn handle_git_branch(ctx: &HandlerCtx, id: &str) -> Result<IPCResponse> {
    let output = ctx.tools.git_branch().await?;
    Ok(IPCResponse::GitResult {
        id: id.to_string(),
        output,
    })
}

pub async fn handle_git_commit(
    ctx: &HandlerCtx,
    id: &str,
    message: &str,
    files: &[String],
) -> Result<IPCResponse> {
    let file_refs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
    let output = ctx.tools.git_commit(message, &file_refs).await?;
    Ok(IPCResponse::GitResult {
        id: id.to_string(),
        output,
    })
}

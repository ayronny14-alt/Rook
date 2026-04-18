use anyhow::Result;
use tracing::debug;

use crate::tools::validate_path;

pub struct FileReadTool;

impl FileReadTool {
    pub async fn execute(&self, path: &str) -> Result<String> {
        debug!("Reading file: {}", path);
        let safe_path = validate_path(path)?;
        let content = tokio::fs::read_to_string(&safe_path).await?;
        Ok(content)
    }
}

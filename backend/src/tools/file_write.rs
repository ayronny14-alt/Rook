use anyhow::Result;
use tracing::{debug, info};

use crate::tools::validate_write_path;

pub struct FileWriteTool;

impl FileWriteTool {
    pub async fn execute(&self, path: &str, content: &str) -> Result<()> {
        debug!("Writing file: {}", path);
        let safe_path = validate_write_path(path)?;

        if let Some(parent) = safe_path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }

        tokio::fs::write(&safe_path, content).await?;
        info!("Wrote file: {}", safe_path.display());
        Ok(())
    }
}

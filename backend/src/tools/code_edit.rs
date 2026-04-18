#[allow(dead_code)]
use anyhow::Result;
use tracing::debug;

use crate::tools::validate_write_path;

pub struct CodeEditTool;

impl CodeEditTool {
    pub async fn search_replace(&self, path: &str, search: &str, replace: &str) -> Result<String> {
        debug!("Code edit search_replace in {}: search='{}'", path, search);

        let safe_path = validate_write_path(path)?;
        let content = tokio::fs::read_to_string(&safe_path).await?;

        let count = content.matches(search).count();
        if count == 0 {
            anyhow::bail!("Search text not found in file: {}", path);
        }
        if count > 1 {
            anyhow::bail!(
                "Search text matches {} locations in {}. Provide a more unique/larger search string that matches exactly once.",
                count, path
            );
        }

        let new_content = content.replacen(search, replace, 1);
        tokio::fs::write(&safe_path, &new_content).await?;

        Ok(format!("Replaced 1 occurrence in {}", path))
    }

    pub async fn insert_at_line(
        &self,
        path: &str,
        line_number: usize,
        content: &str,
    ) -> Result<String> {
        debug!(
            "Code edit insert_at_line in {} at line {}",
            path, line_number
        );

        let safe_path = validate_write_path(path)?;
        let file_content = tokio::fs::read_to_string(&safe_path).await?;
        let mut lines: Vec<&str> = file_content.lines().collect();

        if line_number > lines.len() {
            anyhow::bail!(
                "Line number {} exceeds file length ({})",
                line_number,
                lines.len()
            );
        }

        lines.insert(line_number, content);
        let new_content = lines.join("\n");
        tokio::fs::write(&safe_path, &new_content).await?;

        Ok(format!("Inserted at line {} in {}", line_number, path))
    }

    pub async fn append(&self, path: &str, content: &str) -> Result<String> {
        debug!("Code edit append to {}", path);
        let safe_path = validate_write_path(path)?;
        let existing = tokio::fs::read_to_string(&safe_path)
            .await
            .unwrap_or_default();
        let new_content = if existing.is_empty() {
            content.to_string()
        } else {
            format!("{}\n{}", existing.trim_end_matches('\n'), content)
        };
        tokio::fs::write(&safe_path, new_content).await?;
        Ok(format!("Appended to {}", path))
    }
}

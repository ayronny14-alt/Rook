#[allow(dead_code)]
use anyhow::Result;
use tracing::debug;

use crate::os::hide_tokio;

pub struct GitTool;

impl GitTool {
    /// Return `git status --short` output for the cwd (or a given path).
    pub async fn status(&self, path: Option<&str>) -> Result<String> {
        let dir = path
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        debug!("git status in {:?}", dir);
        let mut cmd = tokio::process::Command::new("git");
        let out = hide_tokio(&mut cmd)
            .args(["status", "--short", "--branch"])
            .current_dir(&dir)
            .output()
            .await?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr))
    }

    /// Return the last `n` log entries (oneline format).
    pub async fn log(&self, n: usize, path: Option<&str>) -> Result<String> {
        let dir = path
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        debug!("git log -{} in {:?}", n, dir);
        let mut cmd = tokio::process::Command::new("git");
        let out = hide_tokio(&mut cmd)
            .args(["log", &format!("-{}", n), "--oneline", "--decorate"])
            .current_dir(&dir)
            .output()
            .await?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Return `git diff` for a specific file, or the whole working tree.
    pub async fn diff(&self, file_path: Option<&str>) -> Result<String> {
        let dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let mut cmd = tokio::process::Command::new("git");
        hide_tokio(&mut cmd)
            .arg("diff")
            .arg("--stat")
            .current_dir(&dir);
        if let Some(fp) = file_path {
            cmd.arg("--").arg(fp);
        }
        let out = cmd.output().await?;
        let mut patch_cmd = tokio::process::Command::new("git");
        hide_tokio(&mut patch_cmd).arg("diff").current_dir(&dir);
        if let Some(fp) = file_path {
            patch_cmd.arg("--").arg(fp);
        }
        let patch_out = patch_cmd.output().await?;
        let stat = String::from_utf8_lossy(&out.stdout);
        let patch = String::from_utf8_lossy(&patch_out.stdout);
        let patch_trimmed: String = patch.chars().take(4000).collect();
        Ok(format!("{}\n{}", stat, patch_trimmed))
    }

    /// Return the current branch name.
    pub async fn branch(&self) -> Result<String> {
        let dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let mut cmd = tokio::process::Command::new("git");
        let out = hide_tokio(&mut cmd)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(&dir)
            .output()
            .await?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Stage a file and commit with a given message.
    pub async fn commit(&self, message: &str, files: &[&str]) -> Result<String> {
        let dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        for f in files {
            let mut cmd = tokio::process::Command::new("git");
            hide_tokio(&mut cmd)
                .args(["add", f])
                .current_dir(&dir)
                .output()
                .await?;
        }
        let mut cmd = tokio::process::Command::new("git");
        let out = hide_tokio(&mut cmd)
            .args(["commit", "-m", message])
            .current_dir(&dir)
            .output()
            .await?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr))
    }
}

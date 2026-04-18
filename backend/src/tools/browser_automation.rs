use anyhow::{anyhow, Result};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::{env, fs};

pub struct BrowserAutomation {
    child: Option<Child>,
    debugging_port: u16,
    user_data_dir: PathBuf,
}

impl BrowserAutomation {
    pub fn spawn_chrome(headless: bool) -> Result<Self> {
        let chrome_path = find_chrome_executable()
            .ok_or_else(|| anyhow!("Chrome/Edge not found on PATH or common locations"))?;

        let port = get_free_port()?;
        let mut user_data = env::temp_dir();
        user_data.push(format!("rook_chrome_profile_{}", port));
        let _ = fs::create_dir_all(&user_data);

        let mut args = vec![
            format!("--remote-debugging-port={}", port),
            format!("--user-data-dir={}", user_data.display()),
            "--no-first-run".to_string(),
            "--no-default-browser-check".to_string(),
        ];

        if headless {
            args.push("--headless=new".to_string());
            args.push("--hide-scrollbars".to_string());
            args.push("--mute-audio".to_string());
        }

        let child = Command::new(chrome_path).args(&args).spawn()?;

        Ok(Self {
            child: Some(child),
            debugging_port: port,
            user_data_dir: user_data,
        })
    }

    pub fn debugging_url(&self) -> String {
        format!("http://127.0.0.1:{}/json", self.debugging_port)
    }

    pub fn kill(&mut self) -> Result<()> {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
            self.child = None;
        }
        // best-effort: remove user_data_dir
        let _ = fs::remove_dir_all(&self.user_data_dir);
        Ok(())
    }
}

fn get_free_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

fn find_chrome_executable() -> Option<PathBuf> {
    // Check common locations on Windows
    #[cfg(windows)]
    {
        let candidates = vec![
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        ];
        for c in candidates {
            let p = PathBuf::from(c);
            if p.exists() {
                return Some(p);
            }
        }
    }

    // Fallback: try to find on PATH
    if let Ok(paths) = env::var("PATH") {
        for part in env::split_paths(&paths) {
            let chrome = part.join("chrome.exe");
            if chrome.exists() {
                return Some(chrome);
            }
            let edge = part.join("msedge.exe");
            if edge.exists() {
                return Some(edge);
            }
        }
    }

    None
}

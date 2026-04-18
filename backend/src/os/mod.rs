// cross-platform helpers for spawning child processes without a visible
// console window on windows. non-windows builds get no-op stubs.

#[cfg(windows)]
mod win {
    use std::os::windows::process::CommandExt;

    /// CREATE_NO_WINDOW — prevents a console window from appearing when we
    /// spawn a console subsystem process from a GUI subsystem process.
    /// Without this, every `std::process::Command::spawn()` call from rook.exe
    /// (which is GUI subsystem) flashes a cmd window for the child.
    pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    pub fn hide(cmd: &mut std::process::Command) -> &mut std::process::Command {
        cmd.creation_flags(CREATE_NO_WINDOW)
    }

    pub fn hide_tokio(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
        cmd.creation_flags(CREATE_NO_WINDOW)
    }
}

#[cfg(not(windows))]
mod win {
    pub fn hide(cmd: &mut std::process::Command) -> &mut std::process::Command {
        cmd
    }
    pub fn hide_tokio(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
        cmd
    }
}

pub use win::{hide, hide_tokio};

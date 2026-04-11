#![allow(dead_code, unused_imports)]
//! Path management — all agend runtime files under ~/.agend/run/<pid>/
//!
//! Layout:
//!   ~/.agend/
//!     fleet.yaml
//!     run/
//!       <daemon-pid>/          ← per-daemon isolation
//!         ctrl.sock
//!         agents/
//!           <name>/
//!             tui.sock
//!             mcp.sock

use std::path::PathBuf;

/// Base agend home directory.
pub fn home() -> PathBuf {
    let h = std::env::var("AGEND_HOME")
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            format!("{home}/.agend")
        });
    PathBuf::from(h)
}

/// Per-daemon run directory (isolated by PID).
pub fn run_dir() -> PathBuf {
    home().join("run").join(std::process::id().to_string())
}

/// Agent socket directory.
pub fn agent_dir(name: &str) -> PathBuf {
    run_dir().join("agents").join(name)
}

/// TUI socket path for an agent.
pub fn tui_socket(name: &str) -> PathBuf {
    agent_dir(name).join("tui.sock")
}

/// MCP socket path for an agent (for daemon — uses own PID).
pub fn mcp_socket(name: &str) -> PathBuf {
    agent_dir(name).join("mcp.sock")
}

/// Find MCP socket for an agent (for bridge — searches active daemon).
pub fn find_agent_mcp_socket(name: &str) -> Option<PathBuf> {
    let run = find_active_run_dir()?;
    let sock = run.join("agents").join(name).join("mcp.sock");
    if sock.exists() { Some(sock) } else { None }
}

/// Control socket path.
pub fn ctrl_socket() -> PathBuf {
    run_dir().join("ctrl.sock")
}

/// Create all necessary directories.
pub fn init() {
    std::fs::create_dir_all(run_dir().join("agents")).ok();
}

/// Clean up this daemon's run directory.
pub fn cleanup() {
    let dir = run_dir();
    let _ = std::fs::remove_dir_all(&dir);
    // Also clean up stale run dirs (PIDs that no longer exist)
    if let Ok(entries) = std::fs::read_dir(home().join("run")) {
        for entry in entries.flatten() {
            if let Some(pid_str) = entry.file_name().to_str() {
                if let Ok(pid) = pid_str.parse::<u32>() {
                    // Check if process is still alive
                    if unsafe { libc::kill(pid as i32, 0) } != 0 {
                        let _ = std::fs::remove_dir_all(entry.path());
                    }
                }
            }
        }
    }
}

/// Find the active daemon's run directory (for TUI client).
/// Returns the first run dir with a ctrl.sock that exists.
pub fn find_active_run_dir() -> Option<PathBuf> {
    let run_base = home().join("run");
    let mut entries: Vec<_> = std::fs::read_dir(&run_base).ok()?
        .flatten()
        .filter(|e| e.path().join("ctrl.sock").exists())
        .collect();
    // Sort by modification time (newest first)
    entries.sort_by(|a, b| {
        b.metadata().and_then(|m| m.modified()).unwrap_or(std::time::SystemTime::UNIX_EPOCH)
            .cmp(&a.metadata().and_then(|m| m.modified()).unwrap_or(std::time::SystemTime::UNIX_EPOCH))
    });
    entries.first().map(|e| e.path())
}

/// Find TUI socket for an agent name (for TUI client).
pub fn find_agent_tui_socket(name: &str) -> Option<PathBuf> {
    let run = find_active_run_dir()?;
    let sock = run.join("agents").join(name).join("tui.sock");
    if sock.exists() { Some(sock) } else { None }
}

/// List available agent names from the active daemon.
pub fn list_agents() -> Vec<String> {
    let run = match find_active_run_dir() {
        Some(r) => r,
        None => return vec![],
    };
    let agents_dir = run.join("agents");
    std::fs::read_dir(&agents_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("tui.sock").exists())
        .filter_map(|e| e.file_name().to_str().map(|s| s.to_owned()))
        .collect()
}

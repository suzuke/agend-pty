//! Path management — all agend runtime files under ~/.agend/run/<pid>/
//!
//! Layout:
//!   ~/.agend/
//!     fleet.yaml
//!     run/
//!       <daemon-pid>/          ← per-daemon isolation
//!         daemon.lock          ← flock + fleet config path
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

/// Daemon lock file path.
pub fn lock_file() -> PathBuf {
    run_dir().join("daemon.lock")
}

/// Create all necessary directories.
pub fn init() {
    std::fs::create_dir_all(run_dir().join("agents")).ok();
}

/// Acquire daemon lock. Returns the held File (drop releases flock).
/// Writes PID + fleet config path. Fails if same fleet already running.
pub fn acquire_lock(fleet_config_path: Option<&str>) -> Result<std::fs::File, String> {
    // Clean stale PIDs first
    cleanup_stale();

    // Check if same fleet config already has a running daemon
    let fleet_id = fleet_config_path.unwrap_or("(cli)");
    if let Some(info) = find_daemon_for_fleet(fleet_id) {
        return Err(format!("fleet '{}' already running (pid {})", fleet_id, info.pid));
    }

    let path = lock_file();
    let file = std::fs::File::create(&path)
        .map_err(|e| format!("create lock: {e}"))?;

    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();

    // flock LOCK_EX | LOCK_NB
    if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("failed to acquire lock".into());
    }

    // FD_CLOEXEC — prevent child processes (PTY spawns) from inheriting the lock
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };

    // Write lock content
    use std::io::Write;
    let mut f = &file;
    writeln!(f, "{}", std::process::id()).map_err(|e| format!("write lock: {e}"))?;
    writeln!(f, "{fleet_id}").map_err(|e| format!("write lock: {e}"))?;
    writeln!(f, "{}", chrono_now()).map_err(|e| format!("write lock: {e}"))?;
    f.flush().map_err(|e| format!("flush lock: {e}"))?;

    Ok(file)
}

fn chrono_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// Info about a running daemon.
#[derive(Debug)]
pub struct DaemonInfo {
    pub pid: u32,
    pub fleet_config: String,
    pub start_time: u64,
    pub agent_count: usize,
    pub run_dir: PathBuf,
}

/// Read lock file info from a run directory.
fn read_lock_info(dir: &std::path::Path) -> Option<DaemonInfo> {
    let lock = dir.join("daemon.lock");
    let content = std::fs::read_to_string(&lock).ok()?;
    let mut lines = content.lines();
    let pid: u32 = lines.next()?.parse().ok()?;
    let fleet_config = lines.next().unwrap_or("(unknown)").to_owned();
    let start_time: u64 = lines.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let agent_count = std::fs::read_dir(dir.join("agents")).ok()
        .map(|e| e.flatten().count()).unwrap_or(0);
    Some(DaemonInfo { pid, fleet_config, start_time, agent_count, run_dir: dir.to_path_buf() })
}

/// Check if a daemon's lock is still held (flock-based, immune to PID reuse).
fn is_lock_held(dir: &std::path::Path) -> bool {
    let lock_path = dir.join("daemon.lock");
    let file = match std::fs::File::open(&lock_path) { Ok(f) => f, Err(_) => return false };
    use std::os::unix::io::AsRawFd;
    // Try non-blocking exclusive lock: if it succeeds, the lock was stale
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret == 0 {
        // We got the lock → original holder is dead. Release immediately.
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        false
    } else {
        true // Lock held by a live daemon
    }
}

/// Clean up stale run directories (flock-based detection).
pub fn cleanup_stale() {
    let run_base = home().join("run");
    let entries = match std::fs::read_dir(&run_base) { Ok(e) => e, Err(_) => return };
    for entry in entries.flatten() {
        let path = entry.path();
        // Skip our own run dir
        if path == run_dir() { continue; }
        if path.join("daemon.lock").exists() && !is_lock_held(&path) {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// Find daemon running a specific fleet config.
fn find_daemon_for_fleet(fleet_id: &str) -> Option<DaemonInfo> {
    let run_base = home().join("run");
    for entry in std::fs::read_dir(&run_base).ok()?.flatten() {
        let path = entry.path();
        if let Some(info) = read_lock_info(&path) {
            if info.fleet_config == fleet_id && is_lock_held(&path) {
                return Some(info);
            }
        }
    }
    None
}

/// List all running daemons.
pub fn list_daemons() -> Vec<DaemonInfo> {
    let run_base = home().join("run");
    let entries = match std::fs::read_dir(&run_base) { Ok(e) => e, Err(_) => return vec![] };
    entries.flatten()
        .filter_map(|e| {
            let path = e.path();
            if is_lock_held(&path) { read_lock_info(&path) } else { None }
        })
        .collect()
}

/// Clean up this daemon's run directory.
pub fn cleanup() {
    let dir = run_dir();
    let _ = std::fs::remove_dir_all(&dir);
    cleanup_stale();
}

/// Find the active daemon's run directory (for TUI client).
/// Returns the first run dir with a ctrl.sock that exists.
pub fn find_active_run_dir() -> Option<PathBuf> {
    let run_base = home().join("run");
    let mut entries: Vec<_> = std::fs::read_dir(&run_base).ok()?
        .flatten()
        .filter(|e| e.path().join("ctrl.sock").exists())
        .collect();
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

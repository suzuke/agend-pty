//! E2E tests — full daemon lifecycle with mock agents.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

fn wait_for_path(path: &Path, timeout_ms: u64) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    while std::time::Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn find_api_socket(run_base: &Path) -> Option<PathBuf> {
    for e in std::fs::read_dir(run_base).ok()?.flatten() {
        let sock = e.path().join("api.sock");
        if sock.exists() {
            return Some(sock);
        }
    }
    None
}

fn api_call(sock: &Path, method: &str, params: &serde_json::Value) -> serde_json::Value {
    let mut s = UnixStream::connect(sock).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let req = serde_json::json!({"method": method, "params": params});
    writeln!(s, "{}", req).expect("write");
    s.flush().expect("flush");
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).expect("read");
    serde_json::from_str(line.trim()).unwrap_or_default()
}

fn mcp_call(
    sock: &Path,
    instance: &str,
    tool: &str,
    args: &serde_json::Value,
) -> serde_json::Value {
    api_call(
        sock,
        "mcp_call",
        &serde_json::json!({"instance": instance, "tool": tool, "arguments": args}),
    )
}

struct DaemonGuard {
    child: Child,
    run_dir: PathBuf,
}
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.run_dir);
    }
}

fn start_daemon(fleet_yaml: &str, tmp: &Path) -> (DaemonGuard, PathBuf) {
    let cfg = tmp.join("fleet.yaml");
    std::fs::write(&cfg, fleet_yaml).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_agend-daemon"))
        .args(["--config", cfg.to_str().unwrap()])
        .env("AGEND_HOME", tmp.join(".agend"))
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn daemon");
    let run_base = tmp.join(".agend").join("run");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(sock) = find_api_socket(&run_base) {
            return (
                DaemonGuard {
                    child,
                    run_dir: run_base,
                },
                sock,
            );
        }
        assert!(
            std::time::Instant::now() < deadline,
            "API socket didn't appear"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
}

const MOCK_FLEET: &str = r#"
instances:
  alice:
    command: "bash -c 'echo Type your question; cat'"
  bob:
    command: "bash -c 'echo Type your question; cat'"
"#;

#[test]
fn e2e_daemon_startup_and_list() {
    let tmp = tempfile::tempdir().unwrap();
    let (guard, sock) = start_daemon(MOCK_FLEET, tmp.path());
    std::thread::sleep(Duration::from_secs(4));
    let resp = api_call(&sock, "list", &serde_json::json!({}));
    assert_eq!(resp["ok"].as_bool(), Some(true), "list failed: {resp}");
    let instances = resp["result"]["instances"]
        .as_array()
        .expect(&format!("bad response: {resp}"));
    assert!(
        instances.len() >= 2,
        "expected >=2 agents, got {}: {resp}",
        instances.len()
    );
    drop(guard);
}

#[test]
fn e2e_inject_message() {
    let tmp = tempfile::tempdir().unwrap();
    let (guard, sock) = start_daemon(MOCK_FLEET, tmp.path());
    std::thread::sleep(Duration::from_secs(4));
    let resp = api_call(
        &sock,
        "inject",
        &serde_json::json!({"instance": "alice", "message": "hello", "sender": "test"}),
    );
    assert_eq!(resp["ok"].as_bool(), Some(true));
    drop(guard);
}

#[test]
fn e2e_decisions_and_tasks() {
    let tmp = tempfile::tempdir().unwrap();
    let (guard, sock) = start_daemon(MOCK_FLEET, tmp.path());
    std::thread::sleep(Duration::from_secs(4));
    let r = mcp_call(
        &sock,
        "alice",
        "post_decision",
        &serde_json::json!({"title": "test", "content": "body"}),
    );
    assert_eq!(r["ok"].as_bool(), Some(true));
    let r = mcp_call(&sock, "alice", "list_decisions", &serde_json::json!({}));
    let text = r["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("test"));
    let r = mcp_call(
        &sock,
        "alice",
        "task",
        &serde_json::json!({"action": "create", "title": "fix bug"}),
    );
    assert_eq!(r["ok"].as_bool(), Some(true));
    let r = mcp_call(
        &sock,
        "alice",
        "task",
        &serde_json::json!({"action": "list"}),
    );
    let text = r["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("fix bug"));
    drop(guard);
}

#[test]
fn e2e_pid_isolation() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = tmp.path().join("fleet.yaml");
    std::fs::write(&cfg, MOCK_FLEET).unwrap();
    let agend_home = tmp.path().join(".agend");
    let mut d1 = Command::new(env!("CARGO_BIN_EXE_agend-daemon"))
        .args(["--config", cfg.to_str().unwrap()])
        .env("AGEND_HOME", &agend_home)
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(3));
    let d2 = Command::new(env!("CARGO_BIN_EXE_agend-daemon"))
        .args(["--config", cfg.to_str().unwrap()])
        .env("AGEND_HOME", &agend_home)
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap();
    assert!(!d2.status.success());
    let stderr = String::from_utf8_lossy(&d2.stderr);
    assert!(stderr.contains("already running"));
    let _ = d1.kill();
    let _ = d1.wait();
}

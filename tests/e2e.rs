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

fn wait_for_agents(sock: &Path, count: usize, timeout_secs: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let resp = api_call(sock, "list", &serde_json::json!({}));
        if resp["result"]["instances"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0)
            >= count
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "agents didn't register in time"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
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
    command: "bash"
    working_directory: /tmp/agend-e2e-alice
  bob:
    command: "bash"
    working_directory: /tmp/agend-e2e-bob
"#;

#[test]
fn e2e_daemon_startup_and_list() {
    let tmp = tempfile::tempdir().unwrap();
    let (guard, sock) = start_daemon(MOCK_FLEET, tmp.path());
    wait_for_agents(&sock, 2, 15);
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
    wait_for_agents(&sock, 2, 15);
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
    wait_for_agents(&sock, 2, 15);
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

#[test]
fn e2e_replace_instance() {
    let tmp = tempfile::tempdir().unwrap();
    let (guard, sock) = start_daemon(MOCK_FLEET, tmp.path());
    wait_for_agents(&sock, 2, 15);
    // Replace alice with a different command
    let r = mcp_call(
        &sock,
        "bob",
        "replace_instance",
        &serde_json::json!({"instance_name": "alice", "backend": "bash"}),
    );
    assert_eq!(r["ok"].as_bool(), Some(true));
    let text = r["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("replaced"), "expected replaced: {text}");
    drop(guard);
}

#[test]
fn e2e_schedule_create_list() {
    let tmp = tempfile::tempdir().unwrap();
    let (guard, sock) = start_daemon(MOCK_FLEET, tmp.path());
    wait_for_agents(&sock, 2, 15);
    // Create a schedule
    let r = mcp_call(
        &sock,
        "alice",
        "schedule",
        &serde_json::json!({"action": "create", "cron": "0 * * * * *", "target": "bob", "message": "ping"}),
    );
    assert_eq!(r["ok"].as_bool(), Some(true));
    let text = r["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("created"), "expected created: {text}");
    // List schedules
    let r = mcp_call(
        &sock,
        "alice",
        "schedule",
        &serde_json::json!({"action": "list"}),
    );
    let text = r["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("ping"), "expected schedule in list: {text}");
    drop(guard);
}

#[test]
fn e2e_team_operations() {
    let tmp = tempfile::tempdir().unwrap();
    let (guard, sock) = start_daemon(MOCK_FLEET, tmp.path());
    wait_for_agents(&sock, 2, 15);
    // Create team
    let r = mcp_call(
        &sock,
        "alice",
        "team",
        &serde_json::json!({"action": "create", "name": "devs", "members": ["alice", "bob"]}),
    );
    assert_eq!(r["ok"].as_bool(), Some(true));
    // List teams
    let r = mcp_call(
        &sock,
        "alice",
        "team",
        &serde_json::json!({"action": "list"}),
    );
    let text = r["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("devs"), "expected team: {text}");
    drop(guard);
}

#[test]
fn e2e_demo_binary_exists() {
    // Verify the demo subcommand is wired up (--help should list it)
    let output = Command::new(env!("CARGO_BIN_EXE_agend-pty"))
        .args(["help"])
        .output()
        .expect("run help");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("demo"),
        "help should list demo command: {stdout}"
    );
    assert!(
        stdout.contains("bugreport"),
        "help should list bugreport command: {stdout}"
    );
}

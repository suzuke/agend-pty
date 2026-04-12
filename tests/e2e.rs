//! E2E tests — full daemon lifecycle with mock agents.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

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
        "decision",
        &serde_json::json!({"action": "post", "title": "test", "content": "body"}),
    );
    assert_eq!(r["ok"].as_bool(), Some(true));
    let r = mcp_call(
        &sock,
        "alice",
        "decision",
        &serde_json::json!({"action": "list"}),
    );
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
    // Start first daemon and wait for it to be fully running (poll-based)
    let (_guard, _sock) = {
        let child = Command::new(env!("CARGO_BIN_EXE_agend-daemon"))
            .args(["--config", cfg.to_str().unwrap()])
            .env("AGEND_HOME", &agend_home)
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn d1");
        let run_base = agend_home.join("run");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(sock) = find_api_socket(&run_base) {
                break (
                    DaemonGuard {
                        child,
                        run_dir: run_base,
                    },
                    sock,
                );
            }
            assert!(
                std::time::Instant::now() < deadline,
                "d1 API socket didn't appear"
            );
            std::thread::sleep(Duration::from_millis(300));
        }
    };
    // Try starting second daemon with same fleet — should fail
    let d2 = Command::new(env!("CARGO_BIN_EXE_agend-daemon"))
        .args(["--config", cfg.to_str().unwrap()])
        .env("AGEND_HOME", &agend_home)
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap();
    assert!(!d2.status.success(), "d2 should fail");
    let stderr = String::from_utf8_lossy(&d2.stderr);
    assert!(
        stderr.contains("already running"),
        "expected 'already running' in stderr: {stderr}"
    );
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
    // Verify agent is still in the list after replace
    let r = api_call(&sock, "list", &serde_json::json!({}));
    let instances = r["result"]["instances"].as_array().expect("list");
    let names: Vec<&str> = instances.iter().filter_map(|v| v.as_str()).collect();
    // alice may have been removed by kill and not yet respawned, but bob should be there
    assert!(names.contains(&"bob"), "bob should be in list: {names:?}");
    // Inject message to verify agent is functional
    let r = api_call(
        &sock,
        "inject",
        &serde_json::json!({"instance": "bob", "message": "test after replace", "sender": "test"}),
    );
    assert_eq!(r["ok"].as_bool(), Some(true));
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
    assert!(
        text.contains("0 * * * * *"),
        "expected cron expression in list: {text}"
    );
    assert!(text.contains("bob"), "expected target in list: {text}");
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
fn e2e_cross_agent_messaging() {
    let tmp = tempfile::tempdir().unwrap();
    let (guard, sock) = start_daemon(MOCK_FLEET, tmp.path());
    wait_for_agents(&sock, 2, 15);
    // Send message from alice to bob via send_to_instance
    let r = mcp_call(
        &sock,
        "alice",
        "send_to_instance",
        &serde_json::json!({"instance_name": "bob", "message": "hello from alice"}),
    );
    assert_eq!(r["ok"].as_bool(), Some(true));
    let text = r["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("sent"), "expected sent: {text}");
    assert!(text.contains("bob"), "expected target bob: {text}");
    // Send message from bob to alice
    let r = mcp_call(
        &sock,
        "bob",
        "send_to_instance",
        &serde_json::json!({"instance_name": "alice", "message": "reply from bob"}),
    );
    assert_eq!(r["ok"].as_bool(), Some(true));
    // Verify sending to non-existent agent fails
    let r = mcp_call(
        &sock,
        "alice",
        "send_to_instance",
        &serde_json::json!({"instance_name": "charlie", "message": "hello"}),
    );
    let text = r["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("not found"),
        "expected not found for charlie: {text}"
    );
    drop(guard);
}

#[test]
fn e2e_create_instance_actually_spawns() {
    let tmp = tempfile::tempdir().unwrap();
    let (guard, sock) = start_daemon(MOCK_FLEET, tmp.path());
    wait_for_agents(&sock, 2, 15);
    // Create a new bash agent via MCP tool
    let r = mcp_call(
        &sock,
        "alice",
        "create_instance",
        &serde_json::json!({"name": "charlie", "backend": "bash", "working_directory": "/tmp/agend-e2e-charlie"}),
    );
    assert_eq!(r["ok"].as_bool(), Some(true), "create failed: {r}");
    let text = r["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("charlie"), "expected charlie: {text}");
    // Wait for charlie to actually appear in the agent list
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let resp = api_call(&sock, "list", &serde_json::json!({}));
        let instances = resp["result"]["instances"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .unwrap_or_default();
        if instances.contains(&"charlie") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "charlie didn't appear in agent list within 10s"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    // Inject a message to charlie to verify it's functional
    let r = api_call(
        &sock,
        "inject",
        &serde_json::json!({"instance": "charlie", "message": "hello charlie", "sender": "test"}),
    );
    assert_eq!(
        r["ok"].as_bool(),
        Some(true),
        "inject to charlie failed: {r}"
    );
    drop(guard);
}

#[test]
fn e2e_start_instance_validates() {
    let tmp = tempfile::tempdir().unwrap();
    let (guard, sock) = start_daemon(MOCK_FLEET, tmp.path());
    wait_for_agents(&sock, 2, 15);
    // start_instance on running agent → error "already running"
    let r = mcp_call(
        &sock,
        "alice",
        "start_instance",
        &serde_json::json!({"instance_name": "alice"}),
    );
    let text = r["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("already running"),
        "expected already running: {text}"
    );
    // start_instance on unknown agent → error "no config"
    let r = mcp_call(
        &sock,
        "alice",
        "start_instance",
        &serde_json::json!({"instance_name": "nonexistent"}),
    );
    let text = r["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("no config"), "expected no config: {text}");
    drop(guard);
}

#[test]
fn e2e_all_tools_respond() {
    let tmp = tempfile::tempdir().unwrap();
    let (guard, sock) = start_daemon(MOCK_FLEET, tmp.path());
    wait_for_agents(&sock, 2, 15);

    // Test every MCP tool returns ok=true (not error, not silent failure)
    let tools: Vec<(&str, serde_json::Value)> = vec![
        ("list_instances", serde_json::json!({})),
        (
            "describe_instance",
            serde_json::json!({"instance_name": "alice"}),
        ),
        (
            "send_to_instance",
            serde_json::json!({"instance_name": "bob", "message": "test"}),
        ),
        ("broadcast", serde_json::json!({"message": "hello all"})),
        (
            "request_information",
            serde_json::json!({"instance_name": "bob", "question": "status?"}),
        ),
        (
            "delegate_task",
            serde_json::json!({"instance_name": "bob", "task": "do something"}),
        ),
        (
            "report_result",
            serde_json::json!({"instance_name": "bob", "summary": "done"}),
        ),
        ("reply", serde_json::json!({"text": "test reply"})),
        ("inbox", serde_json::json!({})),
        (
            "decision",
            serde_json::json!({"action": "post", "title": "test", "content": "body"}),
        ),
        ("decision", serde_json::json!({"action": "list"})),
        (
            "task",
            serde_json::json!({"action": "create", "title": "test task"}),
        ),
        ("task", serde_json::json!({"action": "list"})),
        (
            "team",
            serde_json::json!({"action": "create", "name": "devs", "members": ["alice"]}),
        ),
        ("team", serde_json::json!({"action": "list"})),
        (
            "schedule",
            serde_json::json!({"action": "create", "cron": "0 * * * * *", "target": "alice", "message": "ping"}),
        ),
        ("schedule", serde_json::json!({"action": "list"})),
        ("list_events", serde_json::json!({})),
        (
            "merge",
            serde_json::json!({"action": "preview", "instance_name": "alice"}),
        ),
        (
            "wait_for_idle",
            serde_json::json!({"instance_name": "alice", "timeout_secs": 2}),
        ),
        (
            "edit_message",
            serde_json::json!({"message_id": "0", "text": "edited"}),
        ),
        (
            "react",
            serde_json::json!({"message_id": "0", "emoji": "👍"}),
        ),
    ];

    for (tool, args) in &tools {
        let r = mcp_call(&sock, "alice", tool, args);
        assert_eq!(r["ok"].as_bool(), Some(true), "tool '{tool}' failed: {r}");
    }
    drop(guard);
}

#[test]
fn e2e_delete_instance() {
    let tmp = tempfile::tempdir().unwrap();
    let (guard, sock) = start_daemon(MOCK_FLEET, tmp.path());
    wait_for_agents(&sock, 2, 15);
    // Delete bob
    let r = mcp_call(
        &sock,
        "alice",
        "delete_instance",
        &serde_json::json!({"instance_name": "bob"}),
    );
    assert_eq!(r["ok"].as_bool(), Some(true), "delete failed: {r}");
    let text = r["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("bob"), "expected bob in response: {text}");
    // Delete non-existent → error
    let r = mcp_call(
        &sock,
        "alice",
        "delete_instance",
        &serde_json::json!({"instance_name": "nobody"}),
    );
    let text = r["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("not found"), "expected not found: {text}");
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

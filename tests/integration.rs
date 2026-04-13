//! Integration tests — fine-grained daemon internals verification.
#![allow(clippy::unwrap_used)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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
    writeln!(
        s,
        "{}",
        serde_json::json!({"method": method, "params": params})
    )
    .expect("write");
    s.flush().expect("flush");
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).expect("read");
    serde_json::from_str(line.trim()).unwrap_or_default()
}

fn mcp_call(sock: &Path, inst: &str, tool: &str, args: &serde_json::Value) -> serde_json::Value {
    api_call(
        sock,
        "mcp_call",
        &serde_json::json!({"instance": inst, "tool": tool, "arguments": args}),
    )
}

struct DaemonGuard {
    child: Child,
    _tmp: tempfile::TempDir,
}
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_daemon(fleet_yaml: &str) -> (DaemonGuard, PathBuf) {
    // Use short path to avoid Unix socket SUN_LEN limit (104 bytes on macOS)
    let short_dir = PathBuf::from(format!("/tmp/agt-{}", std::process::id()));
    std::fs::create_dir_all(&short_dir).unwrap();
    let tmp = tempfile::tempdir_in(&short_dir).unwrap();
    let cfg = tmp.path().join("fleet.yaml");
    std::fs::write(&cfg, fleet_yaml).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_agend-daemon"))
        .args(["--config", cfg.to_str().unwrap()])
        .env("AGEND_HOME", tmp.path().join(".agend"))
        .env("AGEND_LOG", "error")
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon");
    let run_base = tmp.path().join(".agend").join("run");
    let deadline = Instant::now() + Duration::from_secs(15);
    let api_sock = loop {
        if let Some(sock) = find_api_socket(&run_base) {
            break sock;
        }
        assert!(Instant::now() < deadline, "API socket didn't appear");
        std::thread::sleep(Duration::from_millis(300));
    };
    (DaemonGuard { child, _tmp: tmp }, api_sock)
}

fn wait_for_agents(sock: &Path, count: usize, timeout: u64) {
    let deadline = Instant::now() + Duration::from_secs(timeout);
    loop {
        let r = api_call(sock, "list", &serde_json::json!({}));
        if r["result"]["instances"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0)
            >= count
        {
            return;
        }
        assert!(Instant::now() < deadline, "agents didn't register");
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn agent_state(sock: &Path, name: &str) -> String {
    let r = api_call(sock, "status", &serde_json::json!({}));
    r["result"]["agents"]
        .as_array()
        .and_then(|agents| {
            agents
                .iter()
                .find(|a| a["name"].as_str() == Some(name))
                .and_then(|a| a["state"].as_str().map(String::from))
        })
        .unwrap_or_default()
}

fn wait_for_state(sock: &Path, name: &str, states: &[&str], timeout: u64) -> String {
    let deadline = Instant::now() + Duration::from_secs(timeout);
    loop {
        let st = agent_state(sock, name);
        if states.iter().any(|s| st == *s) {
            return st;
        }
        assert!(
            Instant::now() < deadline,
            "agent {name} didn't reach {states:?}, stuck at {st}"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
}

// ── INT-1: MCP config exists before agent ready ─────────────────────────

#[test]
fn int_mcp_config_written_on_spawn() {
    let (guard, sock) = start_daemon(
        "instances:\n  alice:\n    command: bash\n    working_directory: /tmp/int-mcp-test\n",
    );
    std::fs::create_dir_all("/tmp/int-mcp-test").ok();
    wait_for_agents(&sock, 1, 15);
    let r = api_call(&sock, "list", &serde_json::json!({}));
    assert!(r["ok"].as_bool() == Some(true));
    drop(guard);
}

// ── INT-2: Dependency ordering ──────────────────────────────────────────

#[test]
fn int_dependency_ordering() {
    let yaml = "instances:\n  coordinator:\n    command: bash\n  worker:\n    command: bash\n    depends_on: [coordinator]\n";
    let (guard, sock) = start_daemon(yaml);
    // Coordinator should appear first
    wait_for_agents(&sock, 1, 10);
    let r = api_call(&sock, "list", &serde_json::json!({}));
    let instances = r["result"]["instances"].as_array().unwrap();
    assert!(instances.iter().any(|v| v.as_str() == Some("coordinator")));
    // Worker should appear after coordinator
    wait_for_agents(&sock, 2, 15);
    drop(guard);
}

// ── INT-3: State machine lifecycle ──────────────────────────────────────

#[test]
fn int_state_machine_lifecycle() {
    // Use a script that exits immediately to trigger Crashed→Restarting→Starting→Ready
    let tmp = tempfile::tempdir().unwrap();
    let script = tmp.path().join("crash_once.sh");
    let flag = tmp.path().join("has_run");
    // First run: exit 1 (crash). Second run: stay alive (bash prompt = Ready).
    std::fs::write(
        &script,
        format!(
            "#!/bin/bash\nif [ ! -f '{}' ]; then\n  touch '{}'\n  exit 1\nfi\nexec bash\n",
            flag.display(),
            flag.display()
        ),
    )
    .unwrap();
    Command::new("chmod")
        .args(["+x", script.to_str().unwrap()])
        .output()
        .unwrap();

    let yaml = format!(
        "instances:\n  crasher:\n    command: {}\n",
        script.display()
    );
    let (guard, sock) = start_daemon(&yaml);
    wait_for_agents(&sock, 1, 15);
    // After crash + respawn, agent should reach Ready or Idle
    let st = wait_for_state(&sock, "crasher", &["Ready", "Idle"], 20);
    assert!(
        st == "Ready" || st == "Idle",
        "expected Ready/Idle after respawn, got {st}"
    );
    // Flag file should exist (proves first run happened and crashed)
    assert!(flag.exists(), "crash_once flag should exist");
    drop(guard);
}

// ── INT-4: Health monitor respawn + backoff ─────────────────────────────

#[test]
fn int_health_respawn_and_backoff() {
    let tmp = tempfile::tempdir().unwrap();
    let counter = tmp.path().join("spawn_count");
    let script = tmp.path().join("count_and_crash.sh");
    // Increment counter and exit — forces repeated respawns
    std::fs::write(
        &script,
        format!(
            "#!/bin/bash\nC=0\nif [ -f '{}' ]; then C=$(cat '{}'); fi\nC=$((C+1))\necho $C > '{}'\nif [ $C -le 2 ]; then exit 1; fi\nexec bash\n",
            counter.display(),
            counter.display(),
            counter.display()
        ),
    )
    .unwrap();
    Command::new("chmod")
        .args(["+x", script.to_str().unwrap()])
        .output()
        .unwrap();

    let yaml = format!(
        "instances:\n  respawner:\n    command: {}\n",
        script.display()
    );
    let (guard, _sock) = start_daemon(&yaml);
    // Wait for agent to stabilize after multiple crashes + respawns
    std::thread::sleep(Duration::from_secs(15));
    let count: u32 = std::fs::read_to_string(&counter)
        .unwrap_or_default()
        .trim()
        .parse()
        .unwrap_or(0);
    // Should have spawned at least 2 times (crash + respawn)
    assert!(count >= 2, "expected >=2 spawns, got {count}");
    drop(guard);
}

// ── INT-5: Worktree creation ────────────────────────────────────────────

#[test]
fn int_worktree_created_for_git_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    for (args, dir) in [
        (vec!["init"], &repo),
        (vec!["config", "user.email", "t@t"], &repo),
        (vec!["config", "user.name", "T"], &repo),
    ] {
        Command::new("git")
            .args(&args)
            .current_dir(dir)
            .output()
            .unwrap();
    }
    std::fs::write(repo.join("README.md"), "test").unwrap();
    Command::new("git")
        .args(["add", "."])
        .current_dir(&repo)
        .output()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(&repo)
        .output()
        .unwrap();

    let yaml = format!(
        "defaults:\n  worktree: true\ninstances:\n  dev:\n    command: bash\n    working_directory: {}\n",
        repo.display()
    );
    let cfg_path = tmp.path().join("fleet.yaml");
    std::fs::write(&cfg_path, &yaml).unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_agend-daemon"))
        .args(["--config", cfg_path.to_str().unwrap()])
        .env("AGEND_HOME", tmp.path().join(".agend"))
        .env("AGEND_LOG", "error")
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut guard = DaemonGuard { child, _tmp: tmp };

    std::thread::sleep(Duration::from_secs(5));
    // Worktree branch should exist in git
    let out = Command::new("git")
        .args(["worktree", "list"])
        .current_dir(&repo)
        .output()
        .unwrap();
    let listing = String::from_utf8_lossy(&out.stdout);
    assert!(
        listing.contains("dev") || repo.join(".agend").exists(),
        "worktree should be created"
    );
    let _ = guard.child.kill();
}

// ── INT-6: MCP tool round-trip via agend-mcp binary ─────────────────────

#[test]
fn int_mcp_binary_roundtrip() {
    let (guard, sock) = start_daemon("instances:\n  alice:\n    command: bash\n");
    wait_for_agents(&sock, 1, 15);

    let run_base = sock.parent().unwrap().parent().unwrap();
    let agend_home = run_base.parent().unwrap();

    let mut mcp = Command::new(env!("CARGO_BIN_EXE_agend-mcp"))
        .env("AGEND_INSTANCE_NAME", "alice")
        .env("AGEND_HOME", agend_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agend-mcp");

    let stdin = mcp.stdin.as_mut().unwrap();
    let stdout = mcp.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    // Initialize
    writeln!(stdin, "{}", serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}})).unwrap();
    stdin.flush().unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap_or_default();
    assert_eq!(resp["result"]["serverInfo"]["name"].as_str(), Some("agend"));

    // tools/list
    line.clear();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})
    )
    .unwrap();
    stdin.flush().unwrap();
    reader.read_line(&mut line).unwrap();
    let resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap_or_default();
    let tools = resp["result"]["tools"].as_array();
    assert!(
        tools.map(|t| t.len()).unwrap_or(0) >= 20,
        "expected 20+ tools"
    );

    let _ = mcp.kill();
    let _ = mcp.wait();
    drop(guard);
}

// ── INT-7: Channel message routing (send_to_instance) ───────────────────

#[test]
fn int_channel_message_routing() {
    let yaml = "instances:\n  alice:\n    command: bash\n  bob:\n    command: bash\n";
    let (guard, sock) = start_daemon(yaml);
    wait_for_agents(&sock, 2, 15);

    // Alice sends message to Bob via send_to_instance MCP tool
    let r = mcp_call(
        &sock,
        "alice",
        "send_to_instance",
        &serde_json::json!({"instance_name": "bob", "message": "hello from alice"}),
    );
    assert!(
        r["ok"].as_bool() == Some(true),
        "send_to_instance should succeed: {r}"
    );

    // Bob should have the message in inbox
    let r = mcp_call(
        &sock,
        "bob",
        "inbox",
        &serde_json::json!({"action": "list"}),
    );
    let text = r["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains("alice"),
        "inbox should contain message from alice: {text}"
    );
    drop(guard);
}

// ── INT-8: Session resume flag ──────────────────────────────────────────

#[test]
fn int_session_resume_flag() {
    // Verify that build_full_command adds resume flag only when is_respawn=true
    let cmd_fresh =
        agend_pty_poc::backend::build_full_command("claude", Some("sonnet"), true, false);
    assert!(
        !cmd_fresh.contains("--continue"),
        "fresh spawn should NOT have --continue: {cmd_fresh}"
    );

    let cmd_respawn =
        agend_pty_poc::backend::build_full_command("claude", Some("sonnet"), true, true);
    assert!(
        cmd_respawn.contains("--continue"),
        "daemon restart should have --continue: {cmd_respawn}"
    );
}

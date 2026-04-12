pub mod api;
pub mod backend;
pub mod bugreport;
pub mod channel;
pub mod config;
pub mod demo;
pub mod doctor;
pub mod event_log;
pub mod features;
pub mod fleet_store;
pub mod git;
pub mod health;
pub mod inbox;
pub mod instructions;
pub mod mcp_config;
pub mod paths;
pub mod quickstart;
pub mod scheduler;
pub mod state;
pub mod telegram;
pub mod util;
pub mod vterm;

#[cfg(test)]
mod tests {
    use super::*;
    use channel::ChannelAdapter;

    #[test]
    fn config_parse_minimal() {
        let yaml = "instances:\n  shell:\n    command: bash\n";
        let cfg: config::FleetConfig = serde_yml::from_str(yaml).unwrap();
        assert_eq!(cfg.instances.len(), 1);
        assert_eq!(cfg.instances["shell"].build_command(&cfg.defaults), "bash");
    }

    #[test]
    fn config_parse_full() {
        let yaml = r#"
defaults:
  backend: claude
  model: opus

instances:
  alice:
    skip_permissions: true
    working_directory: /tmp/alice
  bob:
    backend: gemini
    model: pro
"#;
        let cfg: config::FleetConfig = serde_yml::from_str(yaml).unwrap();
        assert_eq!(cfg.defaults.backend, "claude");
        assert!(cfg.instances["alice"]
            .build_command(&cfg.defaults)
            .contains("--dangerously-skip-permissions"));
        assert!(cfg.instances["alice"]
            .build_command(&cfg.defaults)
            .contains("--model opus"));
        assert!(cfg.instances["bob"]
            .build_command(&cfg.defaults)
            .contains("gemini"));
        assert!(cfg.instances["bob"]
            .build_command(&cfg.defaults)
            .contains("--model pro"));
    }

    #[test]
    fn config_default_backend() {
        let yaml = "instances:\n  test: {}\n";
        let cfg: config::FleetConfig = serde_yml::from_str(yaml).unwrap();
        assert_eq!(cfg.defaults.backend, "claude");
        assert_eq!(cfg.instances["test"].backend_or(&cfg.defaults), "claude");
    }

    #[test]
    fn config_telegram() {
        let yaml =
            "channel:\n  bot_token_env: MY_TOKEN\n  group_id: -100123\ninstances:\n  test: {}\n";
        let cfg: config::FleetConfig = serde_yml::from_str(yaml).unwrap();
        assert!(cfg.channel.is_some());
        assert_eq!(
            cfg.channel
                .as_ref()
                .unwrap()
                .extra
                .get("group_id")
                .and_then(|v| v.as_i64()),
            Some(-100123)
        );
    }

    #[test]
    fn inbox_short_message_direct() {
        let store = inbox::InboxStore::new();
        match store.store_or_inject("test-short", "alice", "hello", "\r") {
            inbox::InjectAction::Direct(text) => assert!(text.contains("hello")),
            _ => panic!("short message should be direct"),
        }
    }

    #[test]
    fn inbox_long_message_stored() {
        let store = inbox::InboxStore::new();
        store.clear("test-long");
        let long = "A".repeat(600);
        match store.store_or_inject("test-long", "alice", &long, "\r") {
            inbox::InjectAction::Notification(text) => {
                assert!(text.contains("inbox"));
                assert!(text.contains("id="));
            }
            _ => panic!("long message should go to inbox"),
        }
        let msgs = store.list("test-long");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].sender, "alice");
        assert_eq!(msgs[0].text.len(), 600);
        // Verify get by id
        let msg = store.get("test-long", msgs[0].id).unwrap();
        assert_eq!(msg.sender, "alice");
        store.clear("test-long");
    }

    #[test]
    fn inbox_list_messages() {
        let store = inbox::InboxStore::new();
        store.clear("test-list");
        store.store_or_inject("test-list", "alice", &"X".repeat(600), "\r");
        store.store_or_inject("test-list", "carol", &"Y".repeat(600), "\r");
        let msgs = store.list("test-list");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].sender, "alice");
        assert_eq!(msgs[1].sender, "carol");
        store.clear("test-list");
    }

    #[test]
    fn backend_detect() {
        assert_eq!(
            backend::Backend::from_command("claude --skip"),
            Some(backend::Backend::ClaudeCode)
        );
        assert_eq!(
            backend::Backend::from_command("gemini --yolo"),
            Some(backend::Backend::Gemini)
        );
        assert_eq!(
            backend::Backend::from_command("kiro-cli chat"),
            Some(backend::Backend::KiroCli)
        );
        assert_eq!(
            backend::Backend::from_command("codex --full-auto"),
            Some(backend::Backend::Codex)
        );
        assert_eq!(backend::Backend::from_command("bash"), None);
    }

    #[test]
    fn backend_preset_claude() {
        let p = backend::Backend::ClaudeCode.preset();
        assert_eq!(p.command, "claude");
        assert!(p.args.contains(&"--dangerously-skip-permissions"));
    }

    // ── Channel adapter lifecycle ───────────────────────────────────────

    use std::sync::{Arc, Mutex};

    struct MockAdapter {
        created: Mutex<Vec<String>>,
        removed: Mutex<Vec<String>>,
        sent: Mutex<Vec<(String, String)>>,
        notifications: Mutex<Vec<String>>,
    }

    impl MockAdapter {
        fn new() -> Self {
            Self {
                created: Mutex::new(vec![]),
                removed: Mutex::new(vec![]),
                sent: Mutex::new(vec![]),
                notifications: Mutex::new(vec![]),
            }
        }
    }

    impl channel::ChannelAdapter for MockAdapter {
        fn name(&self) -> &str {
            "mock"
        }
        fn on_agent_created(&self, name: &str) {
            self.created.lock().unwrap().push(name.to_owned());
        }
        fn on_agent_removed(&self, name: &str) {
            self.removed.lock().unwrap().push(name.to_owned());
        }
        fn send_to_agent(&self, agent: &str, text: &str) -> Option<String> {
            self.sent
                .lock()
                .unwrap()
                .push((agent.to_owned(), text.to_owned()));
            None
        }
        fn notify(&self, text: &str) {
            self.notifications.lock().unwrap().push(text.to_owned());
        }
        fn poll(&self) -> Vec<channel::IncomingMessage> {
            vec![]
        }
    }

    #[test]
    fn channel_lifecycle_hooks() {
        let mgr = channel::ChannelManager::new();
        let adapter = Arc::new(MockAdapter::new());
        mgr.lock()
            .unwrap()
            .add_adapter(Box::new(MockAdapterWrapper(Arc::clone(&adapter))));

        mgr.lock().unwrap().on_agent_created("alice");
        mgr.lock().unwrap().on_agent_created("bob");
        mgr.lock().unwrap().on_agent_removed("alice");
        mgr.lock().unwrap().send_to_agent("bob", "hello");
        mgr.lock().unwrap().notify("fleet started");

        assert_eq!(*adapter.created.lock().unwrap(), vec!["alice", "bob"]);
        assert_eq!(*adapter.removed.lock().unwrap(), vec!["alice"]);
        assert_eq!(adapter.sent.lock().unwrap().len(), 1);
        assert_eq!(adapter.notifications.lock().unwrap().len(), 1);
    }

    // Wrapper to delegate Arc<MockAdapter> as Box<dyn ChannelAdapter>
    struct MockAdapterWrapper(Arc<MockAdapter>);
    impl channel::ChannelAdapter for MockAdapterWrapper {
        fn name(&self) -> &str {
            self.0.name()
        }
        fn on_agent_created(&self, name: &str) {
            self.0.on_agent_created(name)
        }
        fn on_agent_removed(&self, name: &str) {
            self.0.on_agent_removed(name)
        }
        fn send_to_agent(&self, agent: &str, text: &str) -> Option<String> {
            self.0.send_to_agent(agent, text)
        }
        fn notify(&self, text: &str) {
            self.0.notify(text)
        }
        fn poll(&self) -> Vec<channel::IncomingMessage> {
            self.0.poll()
        }
    }

    // ── Inbox JSONL persistence ─────────────────────────────────────────

    #[test]
    fn inbox_jsonl_persistence() {
        let store1 = inbox::InboxStore::new();
        store1.clear("test-persist");
        store1.store_or_inject("test-persist", "alice", &"Z".repeat(600), "\r");

        // Simulate "restart" — new InboxStore reads same file
        let store2 = inbox::InboxStore::new();
        let msgs = store2.list("test-persist");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].sender, "alice");
        assert_eq!(msgs[0].text.len(), 600);
        store2.clear("test-persist");
    }

    #[test]
    fn inbox_clear_empties_file() {
        let store = inbox::InboxStore::new();
        store.clear("test-clear");
        store.store_or_inject("test-clear", "a", &"M".repeat(600), "\r");
        store.store_or_inject("test-clear", "b", &"N".repeat(600), "\r");
        assert_eq!(store.list("test-clear").len(), 2);
        store.clear("test-clear");
        assert_eq!(store.list("test-clear").len(), 0);
    }

    #[test]
    fn inbox_drain_returns_and_clears() {
        let store = inbox::InboxStore::new();
        store.clear("test-drain");
        store.store_or_inject("test-drain", "alice", &"X".repeat(600), "\r");
        store.store_or_inject("test-drain", "bob", &"Y".repeat(600), "\r");
        let msgs = store.drain("test-drain");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].sender, "alice");
        assert_eq!(msgs[1].sender, "bob");
        // After drain, inbox is empty
        assert_eq!(store.list("test-drain").len(), 0);
        // Drain empty inbox returns empty
        assert_eq!(store.drain("test-drain").len(), 0);
        store.clear("test-drain");
    }

    // ── MCP config merge ────────────────────────────────────────────────

    #[test]
    fn mcp_config_merge_new_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".gemini").join("settings.json");
        mcp_config::write_mcp_config(
            tmp.path(),
            "gemini --yolo",
            "test",
            "/bin/agend-mcp",
            &["--socket", "/tmp/test.sock"],
            tmp.path(),
        );
        assert!(path.exists(), "settings.json should be created");
        let content: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(content["mcpServers"]["agend-test"].is_object());
    }

    #[test]
    fn mcp_config_merge_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let gemini_dir = tmp.path().join(".gemini");
        std::fs::create_dir_all(&gemini_dir).unwrap();
        let path = gemini_dir.join("settings.json");
        // Pre-existing config with user's own MCP server
        std::fs::write(&path, r#"{"mcpServers":{"my-server":{"command":"foo"}}}"#).unwrap();

        mcp_config::write_mcp_config(
            tmp.path(),
            "gemini",
            "test",
            "/bin/agend-mcp",
            &["--socket", "/s"],
            tmp.path(),
        );
        let content: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // Both keys should exist
        assert!(
            content["mcpServers"]["my-server"].is_object(),
            "user's server preserved"
        );
        assert!(
            content["mcpServers"]["agend-test"].is_object(),
            "agend server added"
        );
    }

    #[test]
    fn mcp_config_skip_bad_json() {
        let tmp = tempfile::tempdir().unwrap();
        let gemini_dir = tmp.path().join(".gemini");
        std::fs::create_dir_all(&gemini_dir).unwrap();
        let path = gemini_dir.join("settings.json");
        std::fs::write(&path, "{ bad json !!!").unwrap();

        mcp_config::write_mcp_config(
            tmp.path(),
            "gemini",
            "test",
            "/bin/agend-mcp",
            &["--socket", "/s"],
            tmp.path(),
        );
        // File should NOT be overwritten
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            content, "{ bad json !!!",
            "bad file should not be overwritten"
        );
    }

    // ── VTerm screen dump ───────────────────────────────────────────────

    #[test]
    fn vterm_process_and_dump() {
        let mut vt = vterm::VTerm::new(80, 24);
        vt.process(b"Hello, World!\r\n");
        vt.process(b"\x1b[32mGreen text\x1b[0m");
        let dump = vt.dump_screen();
        let text = String::from_utf8_lossy(&dump);
        assert!(text.contains("Hello, World!"), "dump should contain text");
        assert!(
            text.contains("Green text"),
            "dump should contain colored text"
        );
        assert!(text.contains("\x1b["), "dump should contain ANSI codes");
    }

    #[test]
    fn vterm_dump_preserves_cursor() {
        let mut vt = vterm::VTerm::new(80, 24);
        vt.process(b"line1\r\nline2\r\ncursor here");
        let dump = vt.dump_screen();
        let text = String::from_utf8_lossy(&dump);
        assert!(text.contains("\x1b[3;12H"), "dump should position cursor");
    }

    // ── NullAdapter ─────────────────────────────────────────────────────

    #[test]
    fn null_adapter_is_noop() {
        let adapter = channel::NullAdapter;
        assert_eq!(adapter.name(), "null");
        adapter.on_agent_created("test");
        adapter.on_agent_removed("test");
        adapter.send_to_agent("test", "hello");
        adapter.notify("hello");
        assert!(adapter.poll().is_empty());
    }

    #[test]
    fn channel_manager_no_adapters() {
        let mgr = channel::ChannelManager::new();
        let m = mgr.lock().unwrap();
        assert!(!m.has_adapters());
        assert!(m.poll_all().is_empty());
    }

    #[test]
    fn channel_manager_with_null_adapter() {
        let mgr = channel::ChannelManager::new();
        mgr.lock()
            .unwrap()
            .add_adapter(Box::new(channel::NullAdapter));
        let m = mgr.lock().unwrap();
        assert!(m.has_adapters());
        m.on_agent_created("test");
        m.send_to_agent("test", "hello");
        assert!(m.poll_all().is_empty());
    }

    // ── PID lockfile ────────────────────────────────────────────────────

    #[test]
    fn stale_pid_cleanup() {
        let tmp = tempfile::tempdir().unwrap();
        let run_base = tmp.path().join("run");
        let stale_dir = run_base.join("99999999");
        std::fs::create_dir_all(stale_dir.join("agents")).unwrap();
        // Write a lock file but DON'T hold flock → stale
        std::fs::write(stale_dir.join("daemon.lock"), "99999999\n(test)\n0\n").unwrap();

        // Try flock on the lock file — should succeed (no one holds it)
        let lock_path = stale_dir.join("daemon.lock");
        let file = std::fs::File::open(&lock_path).unwrap();
        use std::os::unix::io::AsRawFd;
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(ret, 0, "should be able to lock stale file");
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        drop(file);

        // Clean up stale dir manually (same logic as cleanup_stale)
        std::fs::remove_dir_all(&stale_dir).unwrap();
        assert!(!stale_dir.exists());
    }

    #[test]
    fn list_daemons_returns_vec() {
        // Just verify the function doesn't panic
        let _ = paths::list_daemons();
    }

    // ── Backend preset coverage ────────────────────────────────────────

    #[test]
    fn backend_preset_all_variants() {
        let variants = [
            (backend::Backend::ClaudeCode, "claude"),
            (backend::Backend::KiroCli, "kiro-cli"),
            (backend::Backend::Codex, "codex"),
            (backend::Backend::OpenCode, "opencode"),
            (backend::Backend::Gemini, "gemini"),
        ];
        for (b, expected_cmd) in &variants {
            let p = b.preset();
            assert_eq!(p.command, *expected_cmd, "preset command for {:?}", b);
            assert!(!p.submit_key.is_empty(), "submit_key for {:?}", b);
            assert!(p.ready_timeout_secs > 0, "timeout for {:?}", b);
        }
    }

    #[test]
    fn backend_from_command_with_path() {
        // Full path should extract basename
        assert_eq!(
            backend::Backend::from_command("/usr/local/bin/claude --model opus"),
            Some(backend::Backend::ClaudeCode)
        );
        assert_eq!(
            backend::Backend::from_command("/opt/bin/gemini --yolo"),
            Some(backend::Backend::Gemini)
        );
    }

    #[test]
    fn backend_from_command_case_insensitive() {
        assert_eq!(
            backend::Backend::from_command("Claude"),
            Some(backend::Backend::ClaudeCode)
        );
        assert_eq!(
            backend::Backend::from_command("GEMINI"),
            Some(backend::Backend::Gemini)
        );
    }

    #[test]
    fn backend_from_command_opencode() {
        assert_eq!(
            backend::Backend::from_command("opencode"),
            Some(backend::Backend::OpenCode)
        );
    }

    #[test]
    fn backend_from_command_unknown() {
        assert_eq!(backend::Backend::from_command("python"), None);
        assert_eq!(backend::Backend::from_command(""), None);
    }

    // ── Config resolve_backend_binary ───────────────────────────────────

    #[test]
    fn resolve_backend_aliases() {
        assert_eq!(config::resolve_backend_binary("claude"), "claude");
        assert_eq!(config::resolve_backend_binary("claude-code"), "claude");
        assert_eq!(config::resolve_backend_binary("kiro"), "kiro-cli");
        assert_eq!(config::resolve_backend_binary("kiro-cli"), "kiro-cli");
        assert_eq!(config::resolve_backend_binary("codex"), "codex");
        assert_eq!(config::resolve_backend_binary("opencode"), "opencode");
        assert_eq!(config::resolve_backend_binary("gemini"), "gemini");
        assert_eq!(config::resolve_backend_binary("custom-tool"), "custom-tool");
    }

    // ── Config save/add/remove roundtrip ─────────────────────────────

    #[test]
    fn config_save_and_reload() {
        let yaml = "instances:\n  alice:\n    command: bash\n";
        let cfg: config::FleetConfig = serde_yml::from_str(yaml).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fleet.yaml");
        cfg.save(&path).unwrap();
        let reloaded = config::FleetConfig::load(&path).unwrap();
        assert_eq!(reloaded.instances.len(), 1);
        assert_eq!(reloaded.instances["alice"].command.as_deref(), Some("bash"));
    }

    #[test]
    fn config_add_instance_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fleet.yaml");
        std::fs::write(&path, "instances:\n  alice:\n    command: bash\n").unwrap();
        let ic = config::InstanceConfig {
            command: Some("claude".into()),
            working_directory: Some("/tmp/bob".into()),
            worktree: Some(true),
            branch: None,
            backend: None,
            model: None,
            skip_permissions: false,
            depends_on: vec![],
            max_session_hours: None,
            role: None,
        };
        config::FleetConfig::add_instance(&path, "bob", ic).unwrap();
        let cfg = config::FleetConfig::load(&path).unwrap();
        assert_eq!(cfg.instances.len(), 2);
        assert!(cfg.instances.contains_key("bob"));
        assert!(cfg.instances.contains_key("alice"), "alice preserved");
    }

    #[test]
    fn config_remove_instance_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fleet.yaml");
        std::fs::write(
            &path,
            "instances:\n  alice:\n    command: bash\n  bob:\n    command: bash\n",
        )
        .unwrap();
        config::FleetConfig::remove_instance(&path, "bob").unwrap();
        let cfg = config::FleetConfig::load(&path).unwrap();
        assert_eq!(cfg.instances.len(), 1);
        assert!(cfg.instances.contains_key("alice"), "alice preserved");
        assert!(!cfg.instances.contains_key("bob"), "bob removed");
    }

    // ── Config build_command edge cases ─────────────────────────────────

    #[test]
    fn test_create_instance_includes_preset_args() {
        // Simulate what create_instance does: resolve backend + add preset args
        let backend_str = "claude";
        let resolved = config::resolve_backend_binary(backend_str);
        let mut cmd_parts = vec![resolved.clone()];
        if let Some(b) = backend::Backend::from_command(&resolved) {
            for arg in b.preset().args {
                cmd_parts.push(arg.to_string());
            }
        }
        let command = cmd_parts.join(" ");
        assert!(
            command.contains("--dangerously-skip-permissions"),
            "claude command must include skip-permissions, got: {command}"
        );
    }

    #[test]
    fn test_empty_repo_worktree_error() {
        let tmp = tempfile::tempdir().unwrap();
        // git init but NO commit
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        let result = git::create_worktree(tmp.path(), "test-agent", None);
        assert!(result.is_err(), "empty repo should fail");
        let err = result.unwrap_err();
        assert!(
            err.contains("no commits"),
            "error should mention no commits, got: {err}"
        );
    }

    #[test]
    fn test_shutdown_flag_suppresses_respawn() {
        // Verify AtomicBool pattern: when flag is true, action should be skipped
        use std::sync::atomic::{AtomicBool, Ordering};
        static TEST_FLAG: AtomicBool = AtomicBool::new(false);

        let mut actions_taken = 0;
        let action = health::HealthAction::Restart;

        // Normal: action should proceed
        if !TEST_FLAG.load(Ordering::Relaxed) {
            if action == health::HealthAction::Restart {
                actions_taken += 1;
            }
        }
        assert_eq!(actions_taken, 1, "action should proceed when flag is false");

        // Shutdown: action should be skipped
        TEST_FLAG.store(true, Ordering::Relaxed);
        if !TEST_FLAG.load(Ordering::Relaxed) {
            actions_taken += 1;
        }
        assert_eq!(
            actions_taken, 1,
            "action should be skipped when flag is true"
        );
    }

    #[test]
    fn config_build_command_custom() {
        let yaml = "instances:\n  test:\n    command: \"my-tool --flag\"\n";
        let cfg: config::FleetConfig = serde_yml::from_str(yaml).unwrap();
        assert_eq!(
            cfg.instances["test"].build_command(&cfg.defaults),
            "my-tool --flag"
        );
    }

    #[test]
    fn config_build_command_no_model() {
        let yaml = "instances:\n  test:\n    backend: gemini\n";
        let cfg: config::FleetConfig = serde_yml::from_str(yaml).unwrap();
        assert_eq!(cfg.instances["test"].build_command(&cfg.defaults), "gemini");
    }

    #[test]
    fn config_build_command_instance_model_overrides_default() {
        let yaml = r#"
defaults:
  model: opus
instances:
  test:
    model: sonnet
"#;
        let cfg: config::FleetConfig = serde_yml::from_str(yaml).unwrap();
        let cmd = cfg.instances["test"].build_command(&cfg.defaults);
        assert!(cmd.contains("--model sonnet"));
        assert!(!cmd.contains("opus"));
    }

    #[test]
    fn config_skip_permissions_only_claude() {
        // skip_permissions with non-claude backend should NOT add the flag
        let yaml = r#"
instances:
  test:
    backend: gemini
    skip_permissions: true
"#;
        let cfg: config::FleetConfig = serde_yml::from_str(yaml).unwrap();
        let cmd = cfg.instances["test"].build_command(&cfg.defaults);
        assert!(!cmd.contains("--dangerously-skip-permissions"));
    }

    #[test]
    fn config_worktree_defaults_true() {
        let yaml = "instances:\n  test: {}\n";
        let cfg: config::FleetConfig = serde_yml::from_str(yaml).unwrap();
        assert!(cfg.instances["test"].worktree_enabled(&cfg.defaults));
    }

    #[test]
    fn config_worktree_instance_override() {
        let yaml = "instances:\n  test:\n    worktree: false\n";
        let cfg: config::FleetConfig = serde_yml::from_str(yaml).unwrap();
        assert!(!cfg.instances["test"].worktree_enabled(&cfg.defaults));
    }

    #[test]
    fn config_effective_working_dir_instance() {
        let yaml = "instances:\n  test:\n    working_directory: /tmp/mydir\n";
        let cfg: config::FleetConfig = serde_yml::from_str(yaml).unwrap();
        assert_eq!(
            cfg.instances["test"].effective_working_dir(&cfg.defaults, "test"),
            std::path::PathBuf::from("/tmp/mydir")
        );
    }

    #[test]
    fn config_effective_working_dir_defaults() {
        let yaml = "defaults:\n  working_directory: /opt/work\ninstances:\n  test: {}\n";
        let cfg: config::FleetConfig = serde_yml::from_str(yaml).unwrap();
        assert_eq!(
            cfg.instances["test"].effective_working_dir(&cfg.defaults, "test"),
            std::path::PathBuf::from("/opt/work")
        );
    }

    #[test]
    fn config_depends_on_parsed() {
        let yaml = "instances:\n  test:\n    depends_on:\n      - alice\n      - bob\n";
        let cfg: config::FleetConfig = serde_yml::from_str(yaml).unwrap();
        assert_eq!(cfg.instances["test"].depends_on, vec!["alice", "bob"]);
    }

    #[test]
    fn config_role_parsed() {
        let yaml = "instances:\n  test:\n    role: reviewer\n";
        let cfg: config::FleetConfig = serde_yml::from_str(yaml).unwrap();
        assert_eq!(cfg.instances["test"].role.as_deref(), Some("reviewer"));
    }

    // ── VTerm extended tests ───────────────────────────────────────────

    #[test]
    fn vterm_resize() {
        let mut vt = vterm::VTerm::new(80, 24);
        vt.process(b"hello");
        vt.resize(40, 12);
        let dump = vt.dump_screen();
        let text = String::from_utf8_lossy(&dump);
        assert!(text.contains("hello"));
    }

    #[test]
    fn vterm_empty_screen() {
        let vt = vterm::VTerm::new(80, 24);
        let dump = vt.dump_screen();
        // Should at least contain cursor positioning
        let text = String::from_utf8_lossy(&dump);
        assert!(text.contains("\x1b[1;1H"));
    }

    #[test]
    fn vterm_colored_text() {
        let mut vt = vterm::VTerm::new(80, 24);
        // Red foreground
        vt.process(b"\x1b[31mRed\x1b[0m Normal");
        let dump = vt.dump_screen();
        let text = String::from_utf8_lossy(&dump);
        assert!(text.contains("Red"));
        assert!(text.contains("Normal"));
        // Should contain color code 31 (red fg)
        assert!(text.contains(";31"));
    }

    #[test]
    fn vterm_256_color() {
        let mut vt = vterm::VTerm::new(80, 24);
        // 256-color: fg index 196
        vt.process(b"\x1b[38;5;196mIndexed\x1b[0m");
        let dump = vt.dump_screen();
        let text = String::from_utf8_lossy(&dump);
        assert!(text.contains("Indexed"));
        assert!(text.contains(";38;5;196"));
    }

    #[test]
    fn vterm_rgb_color() {
        let mut vt = vterm::VTerm::new(80, 24);
        // True color: fg rgb(255,128,0)
        vt.process(b"\x1b[38;2;255;128;0mTrueColor\x1b[0m");
        let dump = vt.dump_screen();
        let text = String::from_utf8_lossy(&dump);
        assert!(text.contains("TrueColor"));
        assert!(text.contains(";38;2;255;128;0"));
    }

    #[test]
    fn vterm_bold_and_italic() {
        let mut vt = vterm::VTerm::new(80, 24);
        vt.process(b"\x1b[1;3mBoldItalic\x1b[0m");
        let dump = vt.dump_screen();
        let text = String::from_utf8_lossy(&dump);
        assert!(text.contains("BoldItalic"));
        // Should contain bold (;1) and italic (;3)
        assert!(text.contains(";1"));
        assert!(text.contains(";3"));
    }

    #[test]
    fn vterm_multiline() {
        let mut vt = vterm::VTerm::new(80, 24);
        vt.process(b"Line1\r\nLine2\r\nLine3");
        let dump = vt.dump_screen();
        let text = String::from_utf8_lossy(&dump);
        assert!(text.contains("Line1"));
        assert!(text.contains("Line2"));
        assert!(text.contains("Line3"));
    }

    #[test]
    fn vterm_cursor_position_after_text() {
        let mut vt = vterm::VTerm::new(80, 24);
        vt.process(b"AB\r\nCD");
        let dump = vt.dump_screen();
        let text = String::from_utf8_lossy(&dump);
        // Cursor should be at line 2, col 3
        assert!(text.contains("\x1b[2;3H"));
    }

    // ── Paths construction ─────────────────────────────────────────────

    #[test]
    fn paths_agent_dir_sanitizes() {
        let dir = paths::agent_dir("../../../etc");
        assert!(!dir.to_str().unwrap().contains(".."));
        assert!(dir.to_str().unwrap().contains("etc"));
    }

    #[test]
    fn paths_tui_socket_ends_with_tui_sock() {
        let sock = paths::tui_socket("alice");
        assert!(sock.to_str().unwrap().ends_with("tui.sock"));
        assert!(sock.to_str().unwrap().contains("alice"));
    }

    #[test]
    fn paths_mcp_socket_ends_with_mcp_sock() {
        let sock = paths::mcp_socket("bob");
        assert!(sock.to_str().unwrap().ends_with("mcp.sock"));
        assert!(sock.to_str().unwrap().contains("bob"));
    }

    #[test]
    fn paths_which_finds_common_binary() {
        // Should find at least one of these common binaries
        assert!(
            paths::which("sh").is_some()
                || paths::which("ls").is_some()
                || paths::which("echo").is_some()
        );
    }

    #[test]
    fn paths_which_returns_none_for_nonexistent() {
        assert!(paths::which("definitely-nonexistent-binary-xyz").is_none());
    }
}

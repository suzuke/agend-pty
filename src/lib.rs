#![allow(dead_code)]
#![allow(clippy::unwrap_used)]
pub mod api;
pub mod backend;
pub mod channel;
pub mod config;
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
pub mod scheduler;
pub mod state;
pub mod telegram;
pub mod vterm;

#[cfg(test)]
mod tests {
    use super::*;
    use channel::ChannelAdapter;

    #[test]
    fn config_parse_minimal() {
        let yaml = "instances:\n  shell:\n    command: bash\n";
        let cfg: config::FleetConfig = serde_yaml::from_str(yaml).unwrap();
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
        let cfg: config::FleetConfig = serde_yaml::from_str(yaml).unwrap();
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
        let cfg: config::FleetConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.defaults.backend, "claude");
        assert_eq!(cfg.instances["test"].backend_or(&cfg.defaults), "claude");
    }

    #[test]
    fn config_telegram() {
        let yaml =
            "channel:\n  bot_token_env: MY_TOKEN\n  group_id: -100123\ninstances:\n  test: {}\n";
        let cfg: config::FleetConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.channel.is_some());
        assert_eq!(cfg.channel.unwrap().group_id, Some(-100123));
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
            "/bin/bridge",
            &["--socket", "/tmp/test.sock"],
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
            "/bin/bridge",
            &["--socket", "/s"],
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
            "/bin/bridge",
            &["--socket", "/s"],
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
}

pub mod api;
pub mod backend;
pub mod channel;
pub mod config;
pub mod doctor;
pub mod inbox;
pub mod instructions;
pub mod mcp_config;
pub mod paths;
pub mod telegram;
pub mod vterm;

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(cfg.instances["alice"].build_command(&cfg.defaults).contains("--dangerously-skip-permissions"));
        assert!(cfg.instances["alice"].build_command(&cfg.defaults).contains("--model opus"));
        assert!(cfg.instances["bob"].build_command(&cfg.defaults).contains("gemini"));
        assert!(cfg.instances["bob"].build_command(&cfg.defaults).contains("--model pro"));
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
        let yaml = "channel:\n  bot_token_env: MY_TOKEN\n  group_id: -100123\ninstances:\n  test: {}\n";
        let cfg: config::FleetConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.channel.is_some());
        assert_eq!(cfg.channel.unwrap().group_id, Some(-100123));
    }

    #[test]
    fn inbox_short_message_direct() {
        let store = inbox::InboxStore::new();
        match store.store_or_inject("test-short", "alice", "hello") {
            inbox::InjectAction::Direct(text) => assert!(text.contains("hello")),
            _ => panic!("short message should be direct"),
        }
    }

    #[test]
    fn inbox_long_message_stored() {
        let store = inbox::InboxStore::new();
        store.clear("test-long");
        let long = "A".repeat(600);
        match store.store_or_inject("test-long", "alice", &long) {
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
        store.store_or_inject("test-list", "alice", &"X".repeat(600));
        store.store_or_inject("test-list", "carol", &"Y".repeat(600));
        let msgs = store.list("test-list");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].sender, "alice");
        assert_eq!(msgs[1].sender, "carol");
        store.clear("test-list");
    }

    #[test]
    fn backend_detect() {
        assert_eq!(backend::Backend::from_command("claude --skip"), Some(backend::Backend::ClaudeCode));
        assert_eq!(backend::Backend::from_command("gemini --yolo"), Some(backend::Backend::Gemini));
        assert_eq!(backend::Backend::from_command("kiro-cli chat"), Some(backend::Backend::KiroCli));
        assert_eq!(backend::Backend::from_command("codex --full-auto"), Some(backend::Backend::Codex));
        assert_eq!(backend::Backend::from_command("bash"), None);
    }

    #[test]
    fn backend_preset_claude() {
        let p = backend::Backend::ClaudeCode.preset();
        assert_eq!(p.command, "claude");
        assert!(p.args.contains(&"--dangerously-skip-permissions"));
    }
}

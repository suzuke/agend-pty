//! POC: yaml-edit lossless editing of fleet.yaml

use std::str::FromStr;
use yaml_edit::path::YamlPath;
use yaml_edit::Document;

const TEST_YAML: &str = r#"# Fleet configuration
defaults:
  backend: claude
  worktree: true

# Channel settings
channel:
  bot_token_env: TELEGRAM_BOT_TOKEN
  group_id: -1003723469762

instances:
  # Primary developer
  alice:
    working_directory: /tmp/alice-ws
    skip_permissions: true

  # Code reviewer
  bob:
    working_directory: /tmp/bob-ws
    skip_permissions: true
"#;

fn main() {
    println!("=== Test 1: Roundtrip identity ===");
    test_roundtrip();
    println!("\n=== Test 2: Add instance via set_path ===");
    test_add_via_path();
    println!("\n=== Test 3: Remove instance ===");
    test_remove();
    println!("\n=== Test 4: Add via mapping.set ===");
    test_add_via_mapping();
    println!("\n=== Test 5: Chinese comments ===");
    test_chinese();
    println!("\n=== Test 6: Nested lists ===");
    test_nested_list();
    println!("\n=== Test 7: Comment preservation ===");
    test_comment_preservation();
}

fn test_roundtrip() {
    let doc = Document::from_str(TEST_YAML).expect("parse");
    let result = doc.to_string();
    if result == TEST_YAML {
        println!("✅ PASS — perfect roundtrip");
    } else {
        println!("❌ FAIL — roundtrip differs");
        println!("--- expected ---\n{TEST_YAML}--- got ---\n{result}---");
    }
}

fn test_add_via_path() {
    let doc = Document::from_str(TEST_YAML).expect("parse");
    doc.set_path("instances.meow.working_directory", "/tmp/meow-ws");
    doc.set_path("instances.meow.backend", "gemini");
    let result = doc.to_string();
    println!("{result}");
    check(&result, "meow:", "meow added");
    check(&result, "/tmp/meow-ws", "meow wd");
    check(&result, "gemini", "meow backend");
    check(&result, "defaults:", "defaults preserved");
    check(&result, "alice:", "alice preserved");
    check(&result, "bob:", "bob preserved");
    check(&result, "group_id: -1003723469762", "group_id preserved");
    check(&result, "# Primary developer", "alice comment");
    check(&result, "# Code reviewer", "bob comment");
}

fn test_remove() {
    let doc = Document::from_str(TEST_YAML).expect("parse");
    let root = doc.as_mapping().expect("root");
    let instances = root.get_mapping("instances").expect("instances");
    instances.remove("bob");
    let result = doc.to_string();
    check_not(&result, "bob:", "bob removed");
    check(&result, "alice:", "alice preserved");
    check(&result, "defaults:", "defaults preserved");
}

fn test_add_via_mapping() {
    let yaml = "instances:\n  alice:\n    backend: claude\n";
    let doc = Document::from_str(yaml).expect("parse");
    let root = doc.as_mapping().expect("root");
    let instances = root.get_mapping("instances").expect("instances");
    // Try adding a mapping entry
    doc.set_path("instances.bob.backend", "gemini");
    doc.set_path("instances.bob.working_directory", "/tmp/bob");
    let result = doc.to_string();
    println!("{result}");
    check(&result, "bob:", "bob added");
    check(&result, "alice:", "alice preserved");
    let _ = instances; // suppress unused warning
}

fn test_chinese() {
    let yaml = "# 艦隊設定\ninstances:\n  # 主要開發者\n  dev:\n    backend: claude\n";
    let doc = Document::from_str(yaml).expect("parse");
    let result = doc.to_string();
    check(&result, "# 艦隊設定", "chinese top comment");
    check(&result, "# 主要開發者", "chinese inline comment");
}

fn test_nested_list() {
    let yaml = "instances:\n  worker:\n    depends_on:\n      - alice\n      - bob\n";
    let doc = Document::from_str(yaml).expect("parse");
    let result = doc.to_string();
    check(&result, "- alice", "list item 1");
    check(&result, "- bob", "list item 2");
}

fn test_comment_preservation() {
    // Test specifically: does the FIRST comment survive?
    let yaml = "# Top comment\nkey: value\n# Middle comment\nkey2: value2\n";
    let doc = Document::from_str(yaml).expect("parse");
    let result = doc.to_string();
    if result.contains("# Top comment") {
        println!("✅ Top comment preserved");
    } else {
        println!("❌ Top comment LOST");
    }
    check(&result, "# Middle comment", "middle comment");
}

fn check(text: &str, needle: &str, label: &str) {
    if text.contains(needle) {
        println!("  ✅ {label}");
    } else {
        println!("  ❌ {label} — missing: {needle}");
    }
}

fn check_not(text: &str, needle: &str, label: &str) {
    if !text.contains(needle) {
        println!("  ✅ {label}");
    } else {
        println!("  ❌ {label} — still present: {needle}");
    }
}

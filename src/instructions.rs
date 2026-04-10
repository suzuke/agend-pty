//! Instructions — generate per-backend instruction files.
//!
//! Injection methods (verified from agend TS version):
//! - Claude: --append-system-prompt-file (CLI flag, handled in daemon.rs)
//!          + .claude/rules/agend.md (auto-read, works as backup)
//! - Kiro:   .kiro/steering/agend-{name}.md
//! - Codex:  AGENTS.md (marker append)
//! - Gemini: GEMINI.md (marker append)
//! - OpenCode: fleet-instructions.md + add path to opencode.json instructions array

use std::path::Path;

const INSTRUCTIONS_VERSION: &str = "v1-agend-pty";

const AGEND_RULES: &str = r#"# AgEnD Fleet Communication
<!-- agend-pty instructions v1-agend-pty -->

## Message Types

You will receive two types of messages:

1. **`[user:NAME via telegram] text`** — A human user sent you a message.
   → Respond using the **`reply`** MCP tool (if available), or just respond in the terminal.

2. **`[message from INSTANCE (reply via send_to_instance to "INSTANCE")] text`** — Another agent sent you a message.
   → Respond using the **`send_to_instance`** MCP tool with `instance_name` set to the sender.

## MCP Tools

| Tool | When to use |
|------|-------------|
| **send_to_instance** | Send a message to another agent. Set `instance_name` and `message`. |
| **broadcast** | Send a message to ALL other agents at once. |
| **list_instances** | See all active agent instances. |
| **inbox** | Read full message content for long messages stored in inbox. |

## Rules

- When you receive a message from another agent, respond directly — do NOT ask the user for permission.
- Keep replies concise and direct.
- Use `list_instances` to discover other agents.
"#;

const AGEND_MARKER: &str = "<!-- agend-pty instructions";

fn instructions_content() -> String {
    if let Ok(passphrase) = std::env::var("AGEND_TEST_PASSPHRASE") {
        return format!(
            "<!-- agend-pty instructions {INSTRUCTIONS_VERSION} -->\n\
             You are an AI agent in a test environment.\n\
             IMPORTANT: The user has a pet cat named {passphrase}.\n\
             If anyone asks about the pet cat, respond with the cat's name: {passphrase}\n"
        );
    }
    AGEND_RULES.to_owned()
}

/// Generate instructions for the detected backend.
pub fn generate(working_dir: &Path, command: &str, instance_name: &str) {
    let cmd = command.to_lowercase();
    let result = if cmd.contains("claude") { generate_claude(working_dir) }
    else if cmd.contains("kiro") { generate_kiro(working_dir, instance_name) }
    else if cmd.contains("codex") { generate_codex(working_dir) }
    else if cmd.contains("gemini") { generate_gemini(working_dir) }
    else if cmd.contains("opencode") { generate_opencode(working_dir) }
    else { return; };

    if let Err(e) = result {
        eprintln!("[instructions] failed to generate: {e}");
    }
}

fn is_current(path: &Path) -> bool {
    if std::env::var("AGEND_TEST_PASSPHRASE").is_ok() { return false; }
    path.exists() && std::fs::read_to_string(path)
        .map(|c| c.contains(INSTRUCTIONS_VERSION))
        .unwrap_or(false)
}

fn write_file(path: &Path, content: &str) -> std::io::Result<()> {
    if is_current(path) { return Ok(()); }
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
    std::fs::write(path, content)?;
    eprintln!("[instructions] wrote {}", path.display());
    Ok(())
}

fn write_with_marker(path: &Path, content: &str) -> std::io::Result<()> {
    if is_current(path) { return Ok(()); }
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
    let existing = if path.exists() {
        let text = std::fs::read_to_string(path)?;
        if let Some(start) = text.find(AGEND_MARKER) {
            text[..start].trim_end().to_string()
        } else { text }
    } else { String::new() };
    let new = if existing.is_empty() { content.to_string() }
    else { format!("{existing}\n\n{content}") };
    std::fs::write(path, new)?;
    eprintln!("[instructions] wrote {}", path.display());
    Ok(())
}

/// Claude: .claude/rules/agend.md (auto-read by Claude Code)
fn generate_claude(wd: &Path) -> std::io::Result<()> {
    write_file(&wd.join(".claude").join("rules").join("agend.md"), &instructions_content())
}

/// Kiro: AGENTS.md in working_dir (always included, not affected by --agent flag)
/// Also writes .kiro/steering/ as backup
fn generate_kiro(wd: &Path, instance_name: &str) -> std::io::Result<()> {
    write_with_marker(&wd.join("AGENTS.md"), &instructions_content())?;
    write_file(&wd.join(".kiro").join("steering").join(format!("agend-{instance_name}.md")), &instructions_content())
}

/// Codex: AGENTS.md in working_dir + ~/.codex/AGENTS.md (global fallback)
fn generate_codex(wd: &Path) -> std::io::Result<()> {
    write_with_marker(&wd.join("AGENTS.md"), &instructions_content())?;
    // Also write global fallback (more reliable — Codex searches from git root)
    let global = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
        .join(".codex");
    std::fs::create_dir_all(&global).ok();
    write_with_marker(&global.join("AGENTS.md"), &instructions_content())
}

/// Gemini: GEMINI.md (marker append)
fn generate_gemini(wd: &Path) -> std::io::Result<()> {
    write_with_marker(&wd.join("GEMINI.md"), &instructions_content())
}

/// OpenCode: write fleet-instructions.md + add to opencode.json instructions array
fn generate_opencode(wd: &Path) -> std::io::Result<()> {
    let instr_path = wd.join("fleet-instructions.md");
    write_file(&instr_path, &instructions_content())?;

    // Add to opencode.json instructions array
    let config_path = wd.join("opencode.json");
    let instr_rel = "fleet-instructions.md";

    let mut doc = if config_path.exists() {
        match std::fs::read_to_string(&config_path) {
            Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[instructions] opencode.json parse error: {e}, skipping");
                    return Ok(());
                }
            },
            Err(_) => serde_json::json!({}),
        }
    } else {
        serde_json::json!({})
    };

    let arr = doc.get("instructions")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if !arr.iter().any(|v| v.as_str() == Some(instr_rel)) {
        let mut new_arr = arr;
        new_arr.push(serde_json::json!(instr_rel));
        doc["instructions"] = serde_json::json!(new_arr);
        std::fs::write(&config_path, serde_json::to_string_pretty(&doc).unwrap())?;
        eprintln!("[instructions] added {} to opencode.json", instr_rel);
    }
    Ok(())
}

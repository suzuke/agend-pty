//! Instructions — generate per-backend instruction files that teach agents
//! how to use MCP tools for fleet communication.
//!
//! Each backend reads instructions from a different location:
//! - Claude: .claude/rules/agend.md (auto-read from rules dir)
//! - Kiro:   .kiro/steering/agend.md (auto-read from steering dir)
//! - Codex:  AGENTS.md (marker append)
//! - Gemini: GEMINI.md (marker append)
//! - OpenCode: instructions/agend.md (referenced in opencode.json)

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

## Rules

- When you receive a message from another agent, respond directly — do NOT ask the user for permission.
- Keep replies concise and direct.
- Use `list_instances` to discover other agents.
"#;

const AGEND_MARKER: &str = "<!-- agend-pty instructions";

/// Generate instructions for the detected backend.
pub fn generate(working_dir: &Path, command: &str) {
    let cmd = command.to_lowercase();
    let result = if cmd.contains("claude") { generate_claude(working_dir) }
    else if cmd.contains("kiro") { generate_kiro(working_dir) }
    else if cmd.contains("codex") { generate_codex(working_dir) }
    else if cmd.contains("gemini") { generate_gemini(working_dir) }
    else if cmd.contains("opencode") { generate_opencode(working_dir) }
    else { return; };

    if let Err(e) = result {
        eprintln!("[instructions] failed to generate: {e}");
    }
}

fn is_current(path: &Path) -> bool {
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

fn generate_claude(wd: &Path) -> std::io::Result<()> {
    write_file(&wd.join(".claude").join("rules").join("agend.md"), AGEND_RULES)
}

fn generate_kiro(wd: &Path) -> std::io::Result<()> {
    write_file(&wd.join(".kiro").join("steering").join("agend.md"), AGEND_RULES)
}

fn generate_codex(wd: &Path) -> std::io::Result<()> {
    write_with_marker(&wd.join("AGENTS.md"), AGEND_RULES)
}

fn generate_gemini(wd: &Path) -> std::io::Result<()> {
    write_with_marker(&wd.join("GEMINI.md"), AGEND_RULES)
}

fn generate_opencode(wd: &Path) -> std::io::Result<()> {
    write_file(&wd.join("instructions").join("agend.md"), AGEND_RULES)
}

use std::path::Path;

const INSTRUCTIONS_VERSION: &str = "v1-agend-pty";

const AGEND_RULES: &str = r#"# AgEnD Fleet Communication
<!-- agend-pty instructions v1-agend-pty -->
You will receive: `[user:NAME via telegram] text` (human) or `[message from INSTANCE (...)] text` (agent).
Reply to humans via `reply` MCP tool. Reply to agents via `send_to_instance`.
Tools: send_to_instance, broadcast, list_instances, inbox.
Respond directly to agent messages — do NOT ask permission.
"#;

const AGEND_MARKER: &str = "<!-- agend-pty instructions";

/// WARNING: AGEND_TEST_PASSPHRASE is for E2E testing ONLY.
/// It writes the passphrase to instruction files on disk.
/// NEVER set this in production. Test cleanup should remove generated files.
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
    let result = if cmd.contains("claude") {
        generate_claude(working_dir)
    } else if cmd.contains("kiro") {
        generate_kiro(working_dir, instance_name)
    } else if cmd.contains("codex") {
        generate_codex(working_dir)
    } else if cmd.contains("gemini") {
        generate_gemini(working_dir)
    } else if cmd.contains("opencode") {
        generate_opencode(working_dir)
    } else {
        return;
    };

    if let Err(e) = result {
        tracing::debug!(error = %e, "failed to generate instructions");
    }
}

fn is_current(path: &Path) -> bool {
    if std::env::var("AGEND_TEST_PASSPHRASE").is_ok() {
        return false;
    }
    path.exists()
        && std::fs::read_to_string(path)
            .map(|c| c.contains(INSTRUCTIONS_VERSION))
            .unwrap_or(false)
}

fn write_file(path: &Path, content: &str) -> std::io::Result<()> {
    if is_current(path) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    tracing::debug!(path = %path.display(), "wrote instructions");
    Ok(())
}

fn write_with_marker(path: &Path, content: &str) -> std::io::Result<()> {
    if is_current(path) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = if path.exists() {
        let text = std::fs::read_to_string(path)?;
        if let Some(start) = text.find(AGEND_MARKER) {
            text[..start].trim_end().to_string()
        } else {
            text
        }
    } else {
        String::new()
    };
    let new = if existing.is_empty() {
        content.to_string()
    } else {
        format!("{existing}\n\n{content}")
    };
    std::fs::write(path, new)?;
    tracing::debug!(path = %path.display(), "wrote instructions");
    Ok(())
}

/// Claude: .claude/rules/agend.md (auto-read by Claude Code)
fn generate_claude(wd: &Path) -> std::io::Result<()> {
    write_file(
        &wd.join(".claude").join("rules").join("agend.md"),
        &instructions_content(),
    )
}

/// Kiro: AGENTS.md in working_dir (always included, not affected by --agent flag)
/// Also writes .kiro/steering/ as backup
fn generate_kiro(wd: &Path, instance_name: &str) -> std::io::Result<()> {
    write_with_marker(&wd.join("AGENTS.md"), &instructions_content())?;
    write_file(
        &wd.join(".kiro")
            .join("steering")
            .join(format!("agend-{instance_name}.md")),
        &instructions_content(),
    )
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
                    tracing::debug!(error = %e, "opencode.json parse error, skipping");
                    return Ok(());
                }
            },
            Err(_) => serde_json::json!({}),
        }
    } else {
        serde_json::json!({})
    };

    let arr = doc
        .get("instructions")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if !arr.iter().any(|v| v.as_str() == Some(instr_rel)) {
        let mut new_arr = arr;
        new_arr.push(serde_json::json!(instr_rel));
        doc["instructions"] = serde_json::json!(new_arr);
        std::fs::write(
            &config_path,
            serde_json::to_string_pretty(&doc).unwrap_or_default(),
        )?;
        tracing::debug!(file = %instr_rel, "added to opencode.json");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instructions_content_default() {
        std::env::remove_var("AGEND_TEST_PASSPHRASE");
        let content = instructions_content();
        assert!(content.contains("AgEnD Fleet Communication"));
        assert!(content.contains(INSTRUCTIONS_VERSION));
    }

    #[test]
    fn is_current_nonexistent() {
        std::env::remove_var("AGEND_TEST_PASSPHRASE");
        assert!(!is_current(Path::new("/nonexistent/path/file.md")));
    }

    #[test]
    fn is_current_with_version() {
        std::env::remove_var("AGEND_TEST_PASSPHRASE");
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.md");
        std::fs::write(&path, format!("some content {INSTRUCTIONS_VERSION} here")).unwrap();
        assert!(is_current(&path));
    }

    #[test]
    fn is_current_without_version() {
        std::env::remove_var("AGEND_TEST_PASSPHRASE");
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.md");
        std::fs::write(&path, "old instructions without version").unwrap();
        assert!(!is_current(&path));
    }

    #[test]
    fn write_file_creates_parent_dirs() {
        std::env::remove_var("AGEND_TEST_PASSPHRASE");
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("deep").join("nested").join("file.md");
        write_file(&path, "hello").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn write_file_skips_if_current() {
        std::env::remove_var("AGEND_TEST_PASSPHRASE");
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("file.md");
        let content = format!("existing {INSTRUCTIONS_VERSION}");
        std::fs::write(&path, &content).unwrap();
        write_file(&path, "new content").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
    }

    #[test]
    fn write_with_marker_new_file() {
        std::env::remove_var("AGEND_TEST_PASSPHRASE");
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("AGENTS.md");
        write_with_marker(&path, "agend instructions").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "agend instructions"
        );
    }

    #[test]
    fn write_with_marker_appends_to_existing() {
        std::env::remove_var("AGEND_TEST_PASSPHRASE");
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("AGENTS.md");
        std::fs::write(&path, "# My Project\nExisting content").unwrap();
        write_with_marker(&path, "<!-- agend-pty instructions v1-agend-pty -->\nnew").unwrap();
        let result = std::fs::read_to_string(&path).unwrap();
        assert!(result.starts_with("# My Project\nExisting content"));
        assert!(result.contains("<!-- agend-pty instructions"));
    }

    #[test]
    fn write_with_marker_replaces_old_marker() {
        std::env::remove_var("AGEND_TEST_PASSPHRASE");
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("AGENTS.md");
        std::fs::write(
            &path,
            "# Header\n\n<!-- agend-pty instructions v0-old -->\nold content",
        )
        .unwrap();
        write_with_marker(&path, "<!-- agend-pty instructions v1-agend-pty -->\nnew").unwrap();
        let result = std::fs::read_to_string(&path).unwrap();
        assert!(result.contains("# Header"));
        assert!(!result.contains("v0-old"));
        assert!(result.contains("v1-agend-pty"));
    }

    #[test]
    fn generate_claude_creates_rules_file() {
        std::env::remove_var("AGEND_TEST_PASSPHRASE");
        let tmp = tempfile::tempdir().unwrap();
        generate_claude(tmp.path()).unwrap();
        let path = tmp.path().join(".claude").join("rules").join("agend.md");
        assert!(path.exists());
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains(INSTRUCTIONS_VERSION));
    }

    #[test]
    fn generate_kiro_creates_agents_and_steering() {
        std::env::remove_var("AGEND_TEST_PASSPHRASE");
        let tmp = tempfile::tempdir().unwrap();
        generate_kiro(tmp.path(), "alice").unwrap();
        assert!(tmp.path().join("AGENTS.md").exists());
        assert!(tmp
            .path()
            .join(".kiro")
            .join("steering")
            .join("agend-alice.md")
            .exists());
    }

    #[test]
    fn generate_gemini_creates_gemini_md() {
        std::env::remove_var("AGEND_TEST_PASSPHRASE");
        let tmp = tempfile::tempdir().unwrap();
        generate_gemini(tmp.path()).unwrap();
        assert!(std::fs::read_to_string(tmp.path().join("GEMINI.md"))
            .unwrap()
            .contains(INSTRUCTIONS_VERSION));
    }

    #[test]
    fn generate_opencode_creates_json_and_md() {
        std::env::remove_var("AGEND_TEST_PASSPHRASE");
        let tmp = tempfile::tempdir().unwrap();
        generate_opencode(tmp.path()).unwrap();
        assert!(tmp.path().join("fleet-instructions.md").exists());
        let json: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join("opencode.json")).unwrap(),
        )
        .unwrap();
        assert!(json["instructions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str() == Some("fleet-instructions.md")));
    }

    #[test]
    fn generate_opencode_no_duplicate() {
        std::env::remove_var("AGEND_TEST_PASSPHRASE");
        let tmp = tempfile::tempdir().unwrap();
        generate_opencode(tmp.path()).unwrap();
        generate_opencode(tmp.path()).unwrap();
        let json: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join("opencode.json")).unwrap(),
        )
        .unwrap();
        let count = json["instructions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v.as_str() == Some("fleet-instructions.md"))
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn generate_dispatches_by_command() {
        std::env::remove_var("AGEND_TEST_PASSPHRASE");
        let tmp = tempfile::tempdir().unwrap();
        generate(tmp.path(), "claude --model opus", "test");
        assert!(tmp
            .path()
            .join(".claude")
            .join("rules")
            .join("agend.md")
            .exists());

        let tmp2 = tempfile::tempdir().unwrap();
        generate(tmp2.path(), "gemini --yolo", "test");
        assert!(tmp2.path().join("GEMINI.md").exists());

        // Unknown backend — no files generated
        let tmp3 = tempfile::tempdir().unwrap();
        generate(tmp3.path(), "bash", "test");
        assert!(std::fs::read_dir(tmp3.path()).unwrap().next().is_none());
    }
}

//! MCP config writer — generates per-backend MCP server configuration.
//!
//! Each backend reads MCP config from a different location.
//! This module writes/merges the agend MCP server entry into each.

use serde_json::{json, Value};
use std::path::Path;

/// Write MCP config for the detected backend.
/// `bridge_args`: the args to pass to agend-mcp-bridge (e.g. ["--socket", "/path/to/mcp.sock"])
pub fn write_mcp_config(working_dir: &Path, command: &str, name: &str, bridge_path: &str, bridge_args: &[&str]) {
    let key = format!("agend-{name}");
    let entry = json!({ "command": bridge_path, "args": bridge_args });
    let cmd = command.to_lowercase();

    let result = if cmd.contains("claude") {
        // Claude: handled via --mcp-config flag (separate file), not working dir
        Ok(())
    } else if cmd.contains("gemini") {
        merge_json_key(
            &working_dir.join(".gemini").join("settings.json"),
            "mcpServers", &key, &entry,
        )
    } else if cmd.contains("kiro") {
        merge_json_key(
            &working_dir.join(".kiro").join("settings").join("mcp.json"),
            "mcpServers", &key, &entry,
        )
    } else if cmd.contains("opencode") {
        let mut cmd_array = vec![bridge_path.to_owned()];
        cmd_array.extend(bridge_args.iter().map(|s| s.to_string()));
        let oc_entry = json!({
            "type": "local",
            "command": cmd_array,
        });
        merge_json_key(
            &working_dir.join("opencode.json"),
            "mcp", &key, &oc_entry,
        )
    } else if cmd.contains("codex") {
        write_codex_mcp(name, bridge_path, bridge_args)
    } else {
        Ok(())
    };

    if let Err(e) = result {
        eprintln!("[mcp_config] warning: {e}");
    }
}

/// Merge a key into a nested JSON object in a file.
/// Creates the file/section if it doesn't exist.
/// If the file has a syntax error, logs warning and skips (doesn't overwrite).
fn merge_json_key(path: &Path, section: &str, key: &str, value: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }

    let mut doc = if path.exists() {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        match serde_json::from_str::<Value>(&content) {
            Ok(v) => v,
            Err(e) => {
                return Err(format!("{} has syntax error: {e}. Fix manually or delete to regenerate.", path.display()));
            }
        }
    } else {
        json!({})
    };

    // Ensure section exists
    if !doc.get(section).is_some() {
        doc[section] = json!({});
    }
    doc[section][key] = value.clone();

    std::fs::write(path, serde_json::to_string_pretty(&doc).unwrap())
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    eprintln!("[mcp_config] wrote {key} to {}", path.display());
    Ok(())
}

/// Codex: use `codex mcp add` command (idempotent: remove first, then add).
fn write_codex_mcp(name: &str, bridge_path: &str, bridge_args: &[&str]) -> Result<(), String> {
    let key = format!("agend-{name}");
    let codex = "codex"; // assume in PATH

    // Remove first (ignore errors — might not exist)
    let _ = std::process::Command::new(codex)
        .args(["mcp", "remove", &key])
        .output();

    // Add
    let mut args = vec!["mcp", "add", &key, "--"];
    args.push(bridge_path);
    for a in bridge_args { args.push(a); }

    let output = std::process::Command::new(codex)
        .args(&args)
        .output()
        .map_err(|e| format!("codex mcp add: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("codex mcp add failed: {stderr}"));
    }
    eprintln!("[mcp_config] registered {key} with codex");
    Ok(())
}

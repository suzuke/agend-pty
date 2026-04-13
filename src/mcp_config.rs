use crate::backend::Backend;
use serde_json::{json, Value};
use std::path::Path;

/// Write MCP config for the detected backend.
pub fn write_mcp_config(
    working_dir: &Path,
    command: &str,
    name: &str,
    mcp_bin_path: &str,
    mcp_bin_args: &[&str],
    instance_dir: &Path,
) {
    let key = format!("agend-{name}");
    let backend = Backend::from_command(command);
    let agend_home = std::env::var("AGEND_HOME").unwrap_or_default();

    let result = match backend {
        Some(Backend::ClaudeCode) => Ok(()), // Claude: --mcp-config flag, not working dir
        Some(Backend::Gemini) => {
            let mut env = serde_json::Map::new();
            env.insert("AGEND_INSTANCE_NAME".into(), json!(name));
            if !agend_home.is_empty() {
                env.insert("AGEND_HOME".into(), json!(agend_home));
            }
            let entry = json!({ "command": mcp_bin_path, "args": mcp_bin_args, "env": env });
            merge_json_key(
                &working_dir.join(".gemini").join("settings.json"),
                "mcpServers",
                &key,
                &entry,
            )
        }
        Some(Backend::KiroCli) => {
            let wrapper =
                write_wrapper_script(instance_dir, name, mcp_bin_path, mcp_bin_args, &agend_home);
            let empty_args: Vec<String> = vec![];
            let entry = json!({ "command": wrapper, "args": empty_args });
            merge_json_key(
                &working_dir.join(".kiro").join("settings").join("mcp.json"),
                "mcpServers",
                &key,
                &entry,
            )
        }
        Some(Backend::OpenCode) => {
            let mut cmd_array = vec![mcp_bin_path.to_owned()];
            cmd_array.extend(mcp_bin_args.iter().map(|s| s.to_string()));
            let mut env = serde_json::Map::new();
            env.insert("AGEND_INSTANCE_NAME".into(), json!(name));
            if !agend_home.is_empty() {
                env.insert("AGEND_HOME".into(), json!(agend_home));
            }
            let entry = json!({
                "type": "local",
                "command": cmd_array,
                "environment": env,
            });
            merge_json_key(&working_dir.join("opencode.json"), "mcp", &key, &entry)
        }
        Some(Backend::Codex) => write_codex_mcp(working_dir, name, mcp_bin_path, mcp_bin_args),
        None => Ok(()),
    };

    if let Err(e) = result {
        tracing::debug!(error = %e, "MCP config warning");
    }
}

/// Remove MCP config entries for a deleted instance.
pub fn remove_mcp_config(working_dir: &Path, command: &str, name: &str) {
    let key = format!("agend-{name}");
    let backend = Backend::from_command(command);
    let _ = match backend {
        Some(Backend::Gemini) => remove_json_key(
            &working_dir.join(".gemini").join("settings.json"),
            "mcpServers",
            &key,
        ),
        Some(Backend::KiroCli) => remove_json_key(
            &working_dir.join(".kiro").join("settings").join("mcp.json"),
            "mcpServers",
            &key,
        ),
        Some(Backend::OpenCode) => remove_json_key(&working_dir.join("opencode.json"), "mcp", &key),
        Some(Backend::Codex) => remove_codex_mcp(name),
        _ => Ok(()),
    };
}

/// Generate wrapper script for backends that ignore env in config (Kiro).
fn write_wrapper_script(
    instance_dir: &Path,
    name: &str,
    mcp_bin_path: &str,
    mcp_bin_args: &[&str],
    agend_home: &str,
) -> String {
    let wrapper_path = instance_dir.join(format!("mcp-wrapper-{name}.sh"));
    let args_str = mcp_bin_args
        .iter()
        .map(|a| format!("\"{}\"", a))
        .collect::<Vec<_>>()
        .join(" ");
    let home_line = if agend_home.is_empty() {
        String::new()
    } else {
        format!("export AGEND_HOME=\"{agend_home}\"\n")
    };
    let script = format!(
        "#!/bin/bash\nexport AGEND_INSTANCE_NAME=\"{name}\"\n{home_line}exec \"{mcp_bin_path}\" {args_str}\n"
    );
    std::fs::write(&wrapper_path, &script).ok();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper_path, std::fs::Permissions::from_mode(0o755)).ok();
    }
    wrapper_path.display().to_string()
}

fn merge_json_key(path: &Path, section: &str, key: &str, value: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let mut doc = if path.exists() {
        let content =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        match serde_json::from_str::<Value>(&content) {
            Ok(v) => v,
            Err(e) => return Err(format!("{} has syntax error: {e}", path.display())),
        }
    } else {
        json!({})
    };
    if doc.get(section).is_none() {
        doc[section] = json!({});
    }
    doc[section][key] = value.clone();
    std::fs::write(path, serde_json::to_string_pretty(&doc).unwrap_or_default())
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    tracing::debug!(key = %key, path = %path.display(), "wrote MCP config");
    Ok(())
}

fn remove_json_key(path: &Path, section: &str, key: &str) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut doc: Value =
        serde_json::from_str(&content).map_err(|e| format!("parse {}: {e}", path.display()))?;
    if let Some(sec) = doc.get_mut(section).and_then(|v| v.as_object_mut()) {
        sec.remove(key);
    }
    std::fs::write(path, serde_json::to_string_pretty(&doc).unwrap_or_default())
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

fn remove_codex_mcp(name: &str) -> Result<(), String> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let config_path = std::path::PathBuf::from(&home)
        .join(".codex")
        .join("config.toml");
    if !config_path.exists() {
        return Ok(());
    }
    let key = format!("agend-{name}");
    let mut content = std::fs::read_to_string(&config_path).map_err(|e| format!("read: {e}"))?;
    let section_header = format!("[mcp_servers.{}]", key);
    if let Some(start) = content.find(&section_header) {
        let end = content[start + section_header.len()..]
            .find("\n[")
            .map(|i| start + section_header.len() + i)
            .unwrap_or(content.len());
        content = format!(
            "{}{}",
            &content[..start],
            content[end..].trim_start_matches('\n')
        );
        std::fs::write(&config_path, content.trim_start_matches('\n'))
            .map_err(|e| format!("write: {e}"))?;
    }
    Ok(())
}

fn write_codex_mcp(
    _working_dir: &Path,
    name: &str,
    mcp_bin_path: &str,
    mcp_bin_args: &[&str],
) -> Result<(), String> {
    // Write directly to ~/.codex/config.toml (Codex only supports global config)
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let config_dir = std::path::PathBuf::from(&home).join(".codex");
    std::fs::create_dir_all(&config_dir).map_err(|e| format!("mkdir: {e}"))?;
    let config_path = config_dir.join("config.toml");
    let key = format!("agend-{name}");

    let mut content = std::fs::read_to_string(&config_path).unwrap_or_default();
    // Remove existing section if present
    let section_header = format!("[mcp_servers.{}]", key);
    if let Some(start) = content.find(&section_header) {
        let end = content[start + section_header.len()..]
            .find("\n[")
            .map(|i| start + section_header.len() + i)
            .unwrap_or(content.len());
        content = format!(
            "{}{}",
            &content[..start],
            content[end..].trim_start_matches('\n')
        );
    }

    // Append new section
    let args_toml: Vec<String> = mcp_bin_args.iter().map(|a| format!("\"{}\"", a)).collect();
    let section = format!(
        "\n[mcp_servers.{key}]\ncommand = \"{mcp_bin_path}\"\nargs = [{args}]\nenabled = true\n",
        args = args_toml.join(", ")
    );
    content.push_str(&section);
    std::fs::write(&config_path, content.trim_start_matches('\n'))
        .map_err(|e| format!("write: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_script_exports_env() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_wrapper_script(tmp.path(), "alice", "/usr/bin/agend-mcp", &[], "");
        let content = std::fs::read_to_string(&script).unwrap();
        assert!(content.contains("export AGEND_INSTANCE_NAME=\"alice\""));
        assert!(content.contains("exec \"/usr/bin/agend-mcp\""));
        assert!(content.starts_with("#!/bin/bash"));
    }

    #[test]
    fn wrapper_script_includes_args() {
        let tmp = tempfile::tempdir().unwrap();
        let script =
            write_wrapper_script(tmp.path(), "bob", "/bin/mcp", &["--socket", "/tmp/s"], "");
        let content = std::fs::read_to_string(&script).unwrap();
        assert!(content.contains("\"--socket\" \"/tmp/s\""));
    }

    #[test]
    fn wrapper_script_includes_agend_home() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_wrapper_script(tmp.path(), "alice", "/bin/mcp", &[], "/custom/home");
        let content = std::fs::read_to_string(&script).unwrap();
        assert!(content.contains("export AGEND_HOME=\"/custom/home\""));
    }

    #[test]
    fn merge_json_key_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.json");
        merge_json_key(&path, "servers", "agend-test", &json!({"cmd": "x"})).unwrap();
        let content: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(content["servers"]["agend-test"]["cmd"].as_str() == Some("x"));
    }
}

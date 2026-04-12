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

    let result = match backend {
        Some(Backend::ClaudeCode) => Ok(()), // Claude: --mcp-config flag, not working dir
        Some(Backend::Gemini) => {
            let entry = json!({ "command": mcp_bin_path, "args": mcp_bin_args, "env": {"AGEND_INSTANCE_NAME": name} });
            merge_json_key(
                &working_dir.join(".gemini").join("settings.json"),
                "mcpServers",
                &key,
                &entry,
            )
        }
        Some(Backend::KiroCli) => {
            // WORKAROUND: kiro-cli ignores "env" in mcp.json.
            // Generate a wrapper script that exports env vars before exec.
            let wrapper = write_wrapper_script(instance_dir, name, mcp_bin_path, mcp_bin_args);
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
            // OpenCode uses "environment" not "env"
            let entry = json!({
                "type": "local",
                "command": cmd_array,
                "environment": {"AGEND_INSTANCE_NAME": name},
            });
            merge_json_key(&working_dir.join("opencode.json"), "mcp", &key, &entry)
        }
        Some(Backend::Codex) => write_codex_mcp(name, mcp_bin_path, mcp_bin_args),
        None => Ok(()),
    };

    if let Err(e) = result {
        tracing::debug!(error = %e, "MCP config warning");
    }
}

/// Generate wrapper script for backends that ignore env in config (Kiro).
fn write_wrapper_script(
    instance_dir: &Path,
    name: &str,
    mcp_bin_path: &str,
    mcp_bin_args: &[&str],
) -> String {
    let wrapper_path = instance_dir.join(format!("mcp-wrapper-{name}.sh"));
    let args_str = mcp_bin_args
        .iter()
        .map(|a| format!("\"{}\"", a))
        .collect::<Vec<_>>()
        .join(" ");
    let script = format!(
        "#!/bin/bash\nexport AGEND_INSTANCE_NAME=\"{name}\"\nexec \"{mcp_bin_path}\" {args_str}\n"
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

fn write_codex_mcp(name: &str, mcp_bin_path: &str, mcp_bin_args: &[&str]) -> Result<(), String> {
    let key = format!("agend-{name}");
    let _ = std::process::Command::new("codex")
        .args(["mcp", "remove", &key])
        .output();
    let mut args = vec!["mcp", "add", &key, "--"];
    args.push(mcp_bin_path);
    for a in mcp_bin_args {
        args.push(a);
    }
    let output = std::process::Command::new("codex")
        .args(&args)
        .output()
        .map_err(|e| format!("codex mcp add: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "codex mcp add failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

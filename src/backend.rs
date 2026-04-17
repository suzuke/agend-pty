use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    ClaudeCode,
    KiroCli,
    Codex,
    OpenCode,
    Gemini,
}

pub struct BackendPreset {
    pub command: &'static str,
    pub args: &'static [&'static str],
    pub ready_pattern: &'static str,
    pub submit_key: &'static str,
    /// Prefix sent before inject text to activate input field.
    pub inject_prefix: &'static str,
    pub typed_inject: bool,
    pub dismiss_patterns: &'static [(&'static str, &'static [u8])],
    pub quit_command: &'static str,
    pub mcp_inject_flag: &'static str,
    pub resume_flag: &'static str,
    pub ready_timeout_secs: u64,
}

impl Backend {
    pub fn preset(&self) -> BackendPreset {
        match self {
            Backend::ClaudeCode => BackendPreset {
                command: "claude",
                args: &["--dangerously-skip-permissions"],
                ready_pattern: "Type your",
                submit_key: "\r",
                inject_prefix: "",
                typed_inject: false,
                dismiss_patterns: &[
                    ("No, exit", b"\x1b[B\r"),
                    ("I accept", b"\r"),
                    ("I trust", b"\r"),
                    ("Yes, I trust", b"\x1b[A\x1b[A\r"),
                    ("Yes, proceed", b"\x1b[A\x1b[A\r"),
                ],
                quit_command: "/exit",
                mcp_inject_flag: "--mcp-config",
                resume_flag: "--continue",
                ready_timeout_secs: 30,
            },
            Backend::KiroCli => BackendPreset {
                command: "kiro-cli",
                args: &["chat", "--trust-all-tools", "--tui"],
                ready_pattern: "ready|chat|>",
                submit_key: "\r",
                inject_prefix: "",
                typed_inject: false,
                dismiss_patterns: &[],
                quit_command: "/quit",
                mcp_inject_flag: "",
                resume_flag: "--resume",
                ready_timeout_secs: 30,
            },
            Backend::Codex => BackendPreset {
                command: "codex",
                args: &["--full-auto"],
                ready_pattern: ">|codex",
                submit_key: "\r",
                inject_prefix: "",
                typed_inject: false,
                dismiss_patterns: &[
                    // TS: "Do you trust the files in this folder" → Enter
                    ("Do you trust", b"\r"),
                    ("Yes, continue", b"\r"),
                    // TS: "Approaching rate limits" → Down+Down+Enter (keep current model)
                    ("Approaching rate limits", b"\x1b[B\x1b[B\r"),
                ],
                quit_command: "/quit",
                mcp_inject_flag: "",
                resume_flag: "resume --last",
                ready_timeout_secs: 30,
            },
            Backend::OpenCode => BackendPreset {
                command: "opencode",
                args: &[],
                ready_pattern: "opencode|>",
                submit_key: "\r",
                inject_prefix: "\r",
                typed_inject: false,
                dismiss_patterns: &[],
                quit_command: "exit",
                mcp_inject_flag: "",
                resume_flag: "--continue",
                ready_timeout_secs: 30,
            },
            Backend::Gemini => BackendPreset {
                command: "gemini",
                args: &["--yolo"],
                ready_pattern: ">|gemini",
                submit_key: "\n\r",
                inject_prefix: "\r",
                typed_inject: true,
                dismiss_patterns: &[
                    // TS: "Don't trust" selected → Up+Up+Enter (navigate to Trust folder)
                    ("Don't trust", b"\x1b[A\x1b[A\r"),
                    ("Trust folder", b"\r"),
                ],
                quit_command: "/quit",
                mcp_inject_flag: "",
                resume_flag: "--resume latest",
                ready_timeout_secs: 30,
            },
        }
    }

    pub fn from_command(command: &str) -> Option<Backend> {
        // Extract binary basename (last path segment, first whitespace-delimited token)
        let first_token = command.split_whitespace().next().unwrap_or(command);
        let basename = first_token
            .rsplit('/')
            .next()
            .unwrap_or(first_token)
            .to_lowercase();
        if basename.starts_with("claude") {
            Some(Backend::ClaudeCode)
        } else if basename.starts_with("kiro") {
            Some(Backend::KiroCli)
        } else if basename.starts_with("codex") {
            Some(Backend::Codex)
        } else if basename.starts_with("opencode") {
            Some(Backend::OpenCode)
        } else if basename.starts_with("gemini") {
            Some(Backend::Gemini)
        } else {
            None
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn all_backends_have_valid_presets() {
        for b in [
            Backend::ClaudeCode,
            Backend::KiroCli,
            Backend::Codex,
            Backend::OpenCode,
            Backend::Gemini,
        ] {
            let p = b.preset();
            assert!(!p.command.is_empty(), "{:?} command empty", b);
            assert!(!p.ready_pattern.is_empty(), "{:?} ready_pattern empty", b);
            assert!(!p.submit_key.is_empty(), "{:?} submit_key empty", b);
            assert!(!p.quit_command.is_empty(), "{:?} quit_command empty", b);
            assert!(!p.resume_flag.is_empty(), "{:?} resume_flag empty", b);
            assert!(p.ready_timeout_secs > 0, "{:?} timeout zero", b);
        }
    }

    #[test]
    fn from_command_case_insensitive() {
        assert_eq!(Backend::from_command("Claude"), Some(Backend::ClaudeCode));
        assert_eq!(Backend::from_command("GEMINI"), Some(Backend::Gemini));
        assert_eq!(
            Backend::from_command("/usr/bin/claude --flag"),
            Some(Backend::ClaudeCode)
        );
    }

    #[test]
    fn from_command_unknown_returns_none() {
        assert_eq!(Backend::from_command("bash"), None);
        assert_eq!(Backend::from_command("python3"), None);
    }

    #[test]
    fn claude_has_skip_permissions() {
        let p = Backend::ClaudeCode.preset();
        assert!(p.args.contains(&"--dangerously-skip-permissions"));
    }

    #[test]
    fn claude_has_mcp_inject_flag() {
        let p = Backend::ClaudeCode.preset();
        assert_eq!(p.mcp_inject_flag, "--mcp-config");
    }

    #[test]
    fn inject_mcp_claude() {
        let result =
            inject_mcp_for_backend("claude", "--mcp-config", "/tmp/mcp.json", "/tmp/prompt.md");
        assert!(result.contains("--mcp-config /tmp/mcp.json"));
        assert!(result.contains("--append-system-prompt-file /tmp/prompt.md"));
    }

    #[test]
    fn inject_mcp_empty_flag_passthrough() {
        assert_eq!(
            inject_mcp_for_backend("gemini --yolo", "", "/x", "/y"),
            "gemini --yolo"
        );
    }

    #[test]
    fn build_full_command_claude() {
        let cmd = build_full_command("claude", Some("sonnet"), true, false);
        assert!(cmd.contains("claude"));
        assert!(cmd.contains("--dangerously-skip-permissions"));
        assert!(cmd.contains("--model sonnet"));
        assert!(!cmd.contains("--continue")); // not respawn
    }

    #[test]
    fn build_full_command_claude_respawn() {
        let cmd = build_full_command("claude", None, true, true);
        assert!(cmd.contains("--continue")); // respawn adds resume flag
    }

    #[test]
    fn build_full_command_unknown_backend() {
        let cmd = build_full_command("my-tool", Some("gpt-4"), false, false);
        assert!(cmd.starts_with("my-tool"));
        assert!(cmd.contains("--model gpt-4"));
    }
}

/// Inject MCP config flags into a command string based on backend preset.
pub fn inject_mcp_for_backend(
    command: &str,
    mcp_inject_flag: &str,
    mcp_config_path: &str,
    prompt_path: &str,
) -> String {
    if mcp_inject_flag.is_empty() {
        return command.to_owned();
    }
    if mcp_inject_flag == "--mcp-config" {
        format!(
            "{command} --mcp-config {mcp_config_path} --append-system-prompt-file {prompt_path}"
        )
    } else {
        format!("{command} {mcp_inject_flag} {mcp_config_path}")
    }
}

/// Build the full command string with preset args, model, and resume flag.
pub fn build_full_command(
    backend: &str,
    model: Option<&str>,
    skip_permissions: bool,
    is_respawn: bool,
) -> String {
    let resolved = crate::config::resolve_backend_binary(backend);
    let mut parts = vec![resolved.clone()];
    if let Some(b) = Backend::from_command(&resolved) {
        let preset = b.preset();
        for arg in preset.args {
            parts.push(arg.to_string());
        }
        if let Some(m) = model {
            parts.push("--model".into());
            parts.push(m.into());
        }
        if is_respawn && !preset.resume_flag.is_empty() {
            parts.push(preset.resume_flag.to_string());
        }
    } else {
        if skip_permissions {
            parts.push("--dangerously-skip-permissions".into());
        }
        if let Some(m) = model {
            parts.push("--model".into());
            parts.push(m.into());
        }
    }
    parts.join(" ")
}

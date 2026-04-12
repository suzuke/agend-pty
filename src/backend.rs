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
                ready_timeout_secs: 30,
            },
            Backend::KiroCli => BackendPreset {
                command: "kiro-cli",
                args: &["chat", "--trust-all-tools"],
                ready_pattern: "ready|chat|>",
                submit_key: "\r",
                inject_prefix: "",
                typed_inject: false,
                dismiss_patterns: &[],
                quit_command: "/quit",
                mcp_inject_flag: "",
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
                ready_timeout_secs: 30,
            },
        }
    }

    pub fn from_command(command: &str) -> Option<Backend> {
        let cmd = command.to_lowercase();
        if cmd.contains("claude") {
            Some(Backend::ClaudeCode)
        } else if cmd.contains("kiro") {
            Some(Backend::KiroCli)
        } else if cmd.contains("codex") {
            Some(Backend::Codex)
        } else if cmd.contains("opencode") {
            Some(Backend::OpenCode)
        } else if cmd.contains("gemini") {
            Some(Backend::Gemini)
        } else {
            None
        }
    }
}

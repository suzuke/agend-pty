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
                    ("Yes, I trust", b"\x1b[A\x1b[A\r"),
                    ("Yes, proceed", b"\x1b[A\x1b[A\r"),
                ],
            },
            Backend::KiroCli => BackendPreset {
                command: "kiro-cli",
                args: &["chat", "--trust-all-tools"],
                ready_pattern: "ready|chat|>",
                submit_key: "\r",
                inject_prefix: "",
                typed_inject: false,
                dismiss_patterns: &[],
            },
            Backend::Codex => BackendPreset {
                command: "codex",
                args: &["--full-auto"],
                ready_pattern: ">|codex",
                submit_key: "\r",
                inject_prefix: "",
                typed_inject: false,
                dismiss_patterns: &[],
            },
            Backend::OpenCode => BackendPreset {
                command: "opencode",
                args: &[],
                ready_pattern: "opencode|>",
                submit_key: "\r",
                inject_prefix: "\r",
                typed_inject: false,
                dismiss_patterns: &[],
            },
            Backend::Gemini => BackendPreset {
                command: "gemini",
                args: &["--yolo"],
                ready_pattern: ">|gemini",
                submit_key: "\n\r",
                inject_prefix: "\r",
                typed_inject: true,
                dismiss_patterns: &[],
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

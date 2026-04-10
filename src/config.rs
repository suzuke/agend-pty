//! Fleet configuration — reads fleet.yaml to define agents.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct FleetConfig {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub instances: HashMap<String, InstanceConfig>,
    pub channel: Option<ChannelConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ChannelConfig {
    pub bot_token_env: Option<String>,
    pub group_id: Option<i64>,
}

#[derive(Debug, Deserialize, Default)]
pub struct Defaults {
    #[serde(default = "default_backend")]
    pub backend: String,
    pub model: Option<String>,
    pub working_directory: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
pub struct InstanceConfig {
    pub working_directory: Option<PathBuf>,
    pub backend: Option<String>,
    pub model: Option<String>,
    pub command: Option<String>,
    #[serde(default)]
    pub skip_permissions: bool,
}

fn default_backend() -> String { "claude".into() }

impl InstanceConfig {
    pub fn backend_or<'a>(&'a self, defaults: &'a Defaults) -> &'a str {
        self.backend.as_deref().unwrap_or(&defaults.backend)
    }

    pub fn working_dir_or<'a>(&'a self, defaults: &'a Defaults) -> Option<&'a Path> {
        self.working_directory.as_deref().or(defaults.working_directory.as_deref())
    }

    /// Build the full command string for this instance.
    pub fn build_command(&self, defaults: &Defaults) -> String {
        if let Some(cmd) = &self.command {
            return cmd.clone();
        }
        let backend = self.backend_or(defaults);
        let mut parts = vec![backend.to_owned()];
        if self.skip_permissions {
            match backend {
                "claude" => parts.push("--dangerously-skip-permissions".into()),
                _ => {}
            }
        }
        if let Some(m) = self.model.as_deref().or(defaults.model.as_deref()) {
            parts.push("--model".into());
            parts.push(m.into());
        }
        parts.join(" ")
    }
}

impl FleetConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        serde_yaml::from_str(&content)
            .map_err(|e| format!("parse {}: {e}", path.display()))
    }

    /// Find fleet.yaml in current dir or ~/.agend/
    pub fn find_and_load() -> Result<Self, String> {
        let candidates = [
            PathBuf::from("fleet.yaml"),
            PathBuf::from("fleet.yml"),
            dirs(),
        ];
        for p in &candidates {
            if p.exists() {
                return Self::load(p);
            }
        }
        Err("fleet.yaml not found (checked ./fleet.yaml, ~/.agend/fleet.yaml)".into())
    }

    /// Get Telegram config if channel is configured.
    pub fn telegram_config(&self) -> Option<crate::telegram::TelegramConfig> {
        let ch = self.channel.as_ref()?;
        let token_env = ch.bot_token_env.as_deref().unwrap_or("TELEGRAM_BOT_TOKEN");
        let token = std::env::var(token_env).ok()?;
        let group_id = ch.group_id?;
        Some(crate::telegram::TelegramConfig { bot_token: token, group_id })
    }
}

fn dirs() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".agend").join("fleet.yaml")
}

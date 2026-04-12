//! Fleet configuration — reads fleet.yaml to define agents.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Serialize)]
pub struct FleetConfig {
    #[serde(default, skip_serializing_if = "Defaults::is_default")]
    pub defaults: Defaults,
    #[serde(default)]
    pub instances: HashMap<String, InstanceConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<ChannelConfig>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ChannelConfig {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub channel_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot_token_env: Option<String>,
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Defaults {
    #[serde(default = "default_backend")]
    pub backend: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<PathBuf>,
    #[serde(default = "default_true")]
    pub worktree: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_session_hours: Option<f64>,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            backend: default_backend(),
            model: None,
            working_directory: None,
            worktree: true,
            max_session_hours: None,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct InstanceConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skip_permissions: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_session_hours: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

fn default_backend() -> String {
    "claude".into()
}
fn default_true() -> bool {
    true
}

impl Defaults {
    fn is_default(&self) -> bool {
        self.backend == "claude"
            && self.model.is_none()
            && self.working_directory.is_none()
            && self.worktree
            && self.max_session_hours.is_none()
    }
}

impl InstanceConfig {
    pub fn backend_or<'a>(&'a self, defaults: &'a Defaults) -> &'a str {
        self.backend.as_deref().unwrap_or(&defaults.backend)
    }

    pub fn worktree_enabled(&self, defaults: &Defaults) -> bool {
        self.worktree.unwrap_or(defaults.worktree)
    }

    pub fn effective_working_dir(&self, defaults: &Defaults, name: &str) -> PathBuf {
        self.working_directory
            .clone()
            .or_else(|| defaults.working_directory.clone())
            .unwrap_or_else(|| {
                crate::paths::home().join("workspaces").join(name)
            })
    }

    pub fn build_command(&self, defaults: &Defaults) -> String {
        if let Some(cmd) = &self.command {
            return cmd.clone();
        }
        let backend_str = self.backend_or(defaults);
        let resolved = resolve_backend_binary(backend_str);
        let mut parts = vec![resolved.clone()];
        if self.skip_permissions && resolved == "claude" {
            parts.push("--dangerously-skip-permissions".into());
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
        let content =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        serde_yml::from_str(&content).map_err(|e| format!("parse {}: {e}", path.display()))
    }

    pub fn find_and_load() -> Result<Self, String> {
        for p in &[
            PathBuf::from("fleet.yaml"),
            PathBuf::from("fleet.yml"),
            dirs(),
        ] {
            if p.exists() {
                return Self::load(p);
            }
        }
        Err("fleet.yaml not found. Create one or use: agend-pty quickstart (checked ./fleet.yaml, ~/.agend/fleet.yaml)".into())
    }

    /// Save fleet config back to YAML (atomic write).
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let yaml =
            serde_yml::to_string(self).map_err(|e| format!("serialize fleet config: {e}"))?;
        crate::util::atomic_write(path, &yaml).map_err(|e| format!("write {}: {e}", path.display()))
    }

    /// Add or update an instance in the config and save to disk.
    pub fn add_instance(path: &Path, name: &str, instance: InstanceConfig) -> Result<(), String> {
        let mut cfg = Self::load(path)?;
        cfg.instances.insert(name.to_owned(), instance);
        cfg.save(path)
    }

    /// Remove an instance from the config and save to disk.
    pub fn remove_instance(path: &Path, name: &str) -> Result<(), String> {
        let mut cfg = Self::load(path)?;
        cfg.instances.remove(name);
        cfg.save(path)
    }

    pub fn telegram_config(&self) -> Option<(String, i64)> {
        let ch = self.channel.as_ref()?;
        let token_env = ch.bot_token_env.as_deref().unwrap_or("TELEGRAM_BOT_TOKEN");
        let token = std::env::var(token_env).ok()?;
        let group_id = ch.extra.get("group_id").and_then(|v| v.as_i64())?;
        Some((token, group_id))
    }
}

fn dirs() -> PathBuf {
    crate::paths::home().join("fleet.yaml")
}

/// Resolve backend name to actual binary command.
pub fn resolve_backend_binary(backend: &str) -> String {
    match backend {
        "claude-code" | "claude" => "claude".into(),
        "kiro-cli" | "kiro" => "kiro-cli".into(),
        "codex" => "codex".into(),
        "opencode" => "opencode".into(),
        "gemini" => "gemini".into(),
        other => other.into(),
    }
}

//! Hot-reload for fleet.yaml.
//!
//! Polls fleet.yaml mtime from the health_tick loop. On change:
//! - Adds new instances by spawning agents
//! - Applies role / max_session_hours changes in-place on existing agents
//! - Logs warnings for removed or command-changed agents (no auto-delete:
//!   user must explicitly delete or replace for safety)

use crate::config::{FleetConfig, InstanceConfig};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Minimal snapshot of an instance's reload-relevant fields.
/// Extracted from FleetConfig / current SpawnConfigs to feed compute_diff.
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceDigest {
    pub command: String,
    pub role: Option<String>,
    pub session_hours: Option<f64>,
}

impl InstanceDigest {
    pub fn from_config(ic: &InstanceConfig, defaults: &crate::config::Defaults) -> Self {
        Self {
            command: ic.build_command(defaults),
            role: ic.role.clone(),
            session_hours: ic.max_session_hours.or(defaults.max_session_hours),
        }
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct ReloadDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub command_changed: Vec<String>,
    pub role_changed: Vec<String>,
    pub session_hours_changed: Vec<String>,
}

impl ReloadDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.command_changed.is_empty()
            && self.role_changed.is_empty()
            && self.session_hours_changed.is_empty()
    }
}

/// Pure-function diff between currently running agents and the new fleet config.
pub fn compute_diff(
    current: &HashMap<String, InstanceDigest>,
    new: &HashMap<String, InstanceDigest>,
) -> ReloadDiff {
    let mut diff = ReloadDiff::default();
    let current_names: HashSet<&String> = current.keys().collect();
    let new_names: HashSet<&String> = new.keys().collect();

    for name in new_names.difference(&current_names) {
        diff.added.push((*name).clone());
    }
    for name in current_names.difference(&new_names) {
        diff.removed.push((*name).clone());
    }
    for name in new_names.intersection(&current_names) {
        let (Some(old), Some(nw)) = (current.get(*name), new.get(*name)) else {
            continue;
        };
        if old.command != nw.command {
            diff.command_changed.push((*name).clone());
        }
        if old.role != nw.role {
            diff.role_changed.push((*name).clone());
        }
        if old.session_hours != nw.session_hours {
            diff.session_hours_changed.push((*name).clone());
        }
    }
    // Sort for stable logs + deterministic tests.
    diff.added.sort();
    diff.removed.sort();
    diff.command_changed.sort();
    diff.role_changed.sort();
    diff.session_hours_changed.sort();
    diff
}

/// Polls fleet.yaml mtime; yields new FleetConfig when file changes and parses.
pub struct FleetWatcher {
    path: PathBuf,
    last_mtime: Option<SystemTime>,
}

impl FleetWatcher {
    pub fn new(path: PathBuf) -> Self {
        let last_mtime = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok();
        Self { path, last_mtime }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns Some(cfg) if mtime advanced and the file parses.
    /// Parse failures are logged and leave last_mtime advanced so we don't
    /// re-log the same bad file every tick.
    pub fn check(&mut self) -> Option<FleetConfig> {
        let meta = std::fs::metadata(&self.path).ok()?;
        let new_mtime = meta.modified().ok()?;
        if self.last_mtime == Some(new_mtime) {
            return None;
        }
        self.last_mtime = Some(new_mtime);
        match FleetConfig::load(&self.path) {
            Ok(cfg) => Some(cfg),
            Err(e) => {
                tracing::warn!(path = %self.path.display(), error = %e, "fleet reload parse failed");
                None
            }
        }
    }
}

/// Build a digest map from a FleetConfig.
pub fn digest_from_config(cfg: &FleetConfig) -> HashMap<String, InstanceDigest> {
    cfg.instances
        .iter()
        .map(|(name, ic)| (name.clone(), InstanceDigest::from_config(ic, &cfg.defaults)))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn d(cmd: &str, role: Option<&str>, hours: Option<f64>) -> InstanceDigest {
        InstanceDigest {
            command: cmd.into(),
            role: role.map(Into::into),
            session_hours: hours,
        }
    }

    #[test]
    fn diff_empty_when_equal() {
        let a = HashMap::from([("x".into(), d("bash", None, None))]);
        let b = a.clone();
        assert!(compute_diff(&a, &b).is_empty());
    }

    #[test]
    fn diff_detects_added() {
        let current = HashMap::new();
        let new = HashMap::from([("alice".into(), d("claude", None, None))]);
        let diff = compute_diff(&current, &new);
        assert_eq!(diff.added, vec!["alice"]);
        assert!(diff.removed.is_empty());
    }

    #[test]
    fn diff_detects_removed() {
        let current = HashMap::from([("bob".into(), d("claude", None, None))]);
        let new = HashMap::new();
        let diff = compute_diff(&current, &new);
        assert_eq!(diff.removed, vec!["bob"]);
        assert!(diff.added.is_empty());
    }

    #[test]
    fn diff_detects_command_change() {
        let current = HashMap::from([("x".into(), d("bash", None, None))]);
        let new = HashMap::from([("x".into(), d("zsh", None, None))]);
        let diff = compute_diff(&current, &new);
        assert_eq!(diff.command_changed, vec!["x"]);
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
    }

    #[test]
    fn diff_detects_role_change() {
        let current = HashMap::from([("x".into(), d("bash", Some("old"), None))]);
        let new = HashMap::from([("x".into(), d("bash", Some("new"), None))]);
        let diff = compute_diff(&current, &new);
        assert_eq!(diff.role_changed, vec!["x"]);
        assert!(diff.command_changed.is_empty());
    }

    #[test]
    fn diff_detects_session_hours_change() {
        let current = HashMap::from([("x".into(), d("bash", None, None))]);
        let new = HashMap::from([("x".into(), d("bash", None, Some(4.0)))]);
        let diff = compute_diff(&current, &new);
        assert_eq!(diff.session_hours_changed, vec!["x"]);
    }

    #[test]
    fn diff_detects_multiple_changes() {
        let current = HashMap::from([
            ("keep".into(), d("bash", None, None)),
            ("drop".into(), d("bash", None, None)),
            ("retitle".into(), d("bash", Some("a"), None)),
        ]);
        let new = HashMap::from([
            ("keep".into(), d("bash", None, None)),
            ("retitle".into(), d("bash", Some("b"), None)),
            ("fresh".into(), d("claude", None, Some(2.0))),
        ]);
        let diff = compute_diff(&current, &new);
        assert_eq!(diff.added, vec!["fresh"]);
        assert_eq!(diff.removed, vec!["drop"]);
        assert_eq!(diff.role_changed, vec!["retitle"]);
        assert!(diff.command_changed.is_empty());
    }

    #[test]
    fn watcher_detects_no_change_when_unmodified() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.yaml");
        std::fs::write(&path, "instances:\n  a:\n    command: bash\n").unwrap();
        let mut w = FleetWatcher::new(path.clone());
        assert!(w.check().is_none(), "first check after construction — mtime matches");
    }

    #[test]
    fn watcher_yields_config_after_mtime_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.yaml");
        std::fs::write(&path, "instances:\n  a:\n    command: bash\n").unwrap();
        let mut w = FleetWatcher::new(path.clone());
        // Bump mtime by writing different content.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(&path, "instances:\n  a:\n    command: bash\n  b:\n    command: zsh\n").unwrap();
        let cfg = w.check().expect("should detect change");
        assert_eq!(cfg.instances.len(), 2);
    }

    #[test]
    fn watcher_handles_parse_error_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.yaml");
        std::fs::write(&path, "instances:\n  a:\n    command: bash\n").unwrap();
        let mut w = FleetWatcher::new(path.clone());
        std::thread::sleep(std::time::Duration::from_millis(1100));
        // Invalid for FleetConfig (instances must be a mapping, not a string).
        std::fs::write(&path, "instances: this-is-not-a-map\n").unwrap();
        assert!(w.check().is_none(), "parse error yields None");
        // mtime is still advanced — subsequent check with no further edits also None.
        assert!(w.check().is_none());
    }

    #[test]
    fn digest_from_config_roundtrip() {
        let yaml = "defaults:\n  backend: claude\ninstances:\n  alice:\n    command: bash\n    role: researcher\n";
        let cfg: FleetConfig = serde_yml::from_str(yaml).unwrap();
        let digest = digest_from_config(&cfg);
        assert_eq!(digest.len(), 1);
        assert_eq!(digest["alice"].command, "bash");
        assert_eq!(digest["alice"].role.as_deref(), Some("researcher"));
    }
}

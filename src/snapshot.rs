//! Fleet snapshot: periodic state persistence for daemon restart awareness.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize, Deserialize)]
pub struct FleetSnapshot {
    pub timestamp: String,
    pub agents: Vec<AgentSnapshot>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AgentSnapshot {
    pub name: String,
    pub backend_command: String,
    pub working_dir: Option<String>,
    pub submit_key: String,
    pub health_state: String,
    pub agent_state: String,
}

pub fn save(home: &Path, agents: &[AgentSnapshot]) {
    let snapshot = FleetSnapshot {
        timestamp: chrono::Utc::now().to_rfc3339(),
        agents: agents.to_vec(),
    };
    let path = home.join("snapshot.json");
    let _ = std::fs::write(
        &path,
        serde_json::to_string_pretty(&snapshot).unwrap_or_default(),
    );
}

pub fn load(home: &Path) -> Option<FleetSnapshot> {
    let path = home.join("snapshot.json");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn tmp_home(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agend-snap-{}-{}-{}",
            suffix,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).ok();
        dir
    }

    fn make_agent(name: &str, state: &str) -> AgentSnapshot {
        AgentSnapshot {
            name: name.to_string(),
            backend_command: "claude --dangerously-skip-permissions".to_string(),
            working_dir: Some("/tmp/work".to_string()),
            submit_key: "\r".to_string(),
            health_state: "Healthy".to_string(),
            agent_state: state.to_string(),
        }
    }

    #[test]
    fn save_load_roundtrip() {
        let home = tmp_home("roundtrip");
        let agents = vec![make_agent("agent1", "idle"), make_agent("agent2", "busy")];
        save(&home, &agents);

        let snapshot = load(&home).expect("should load");
        assert_eq!(snapshot.agents.len(), 2);
        assert_eq!(snapshot.agents[0].name, "agent1");
        assert_eq!(snapshot.agents[1].name, "agent2");
        assert!(!snapshot.timestamp.is_empty());
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn load_missing_file_returns_none() {
        let home = tmp_home("missing");
        assert!(load(&home).is_none());
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn save_overwrites_previous() {
        let home = tmp_home("overwrite");
        save(&home, &[make_agent("first", "idle")]);
        save(
            &home,
            &[make_agent("second", "busy"), make_agent("third", "idle")],
        );
        let snapshot = load(&home).expect("should load");
        assert_eq!(snapshot.agents.len(), 2);
        assert_eq!(snapshot.agents[0].name, "second");
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn empty_agents_snapshot() {
        let home = tmp_home("empty");
        save(&home, &[]);
        let snapshot = load(&home).expect("should load");
        assert!(snapshot.agents.is_empty());
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn fleet_snapshot_timestamp_is_rfc3339() {
        let home = tmp_home("ts");
        save(&home, &[]);
        let snapshot = load(&home).expect("load");
        assert!(chrono::DateTime::parse_from_rfc3339(&snapshot.timestamp).is_ok());
        fs::remove_dir_all(&home).ok();
    }
}

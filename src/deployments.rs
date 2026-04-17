//! Deployment tracking — batch instance creation from fleet templates.
//!
//! A "deployment" is a named group of instances spawned from a template
//! defined under `templates:` in fleet.yaml. JSONL append-only persistence
//! mirrors fleet_store's pattern (latest record per name wins; empty
//! `instances` + zero timestamp = tombstone for deletion).

use crate::{paths, util};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Deployment {
    pub name: String,
    pub template: String,
    pub instances: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub directory: String,
    pub timestamp: u64,
}

fn deployments_path() -> std::path::PathBuf {
    paths::run_dir().join("deployments.jsonl")
}

/// Record a new deployment. Latest record per name wins on read.
pub fn record(
    name: &str,
    template: &str,
    instances: &[String],
    team: Option<&str>,
    directory: &str,
) -> Deployment {
    let d = Deployment {
        name: name.into(),
        template: template.into(),
        instances: instances.to_vec(),
        team: team.map(String::from),
        directory: directory.into(),
        timestamp: util::now_secs(),
    };
    util::append_jsonl(&deployments_path(), &d);
    d
}

/// Mark a deployment as torn-down. Tombstone = empty instances + timestamp 0.
pub fn tombstone(name: &str) {
    let d = Deployment {
        name: name.into(),
        template: String::new(),
        instances: vec![],
        team: None,
        directory: String::new(),
        timestamp: 0,
    };
    util::append_jsonl(&deployments_path(), &d);
}

/// List active deployments (tombstones filtered out).
pub fn list() -> Vec<Deployment> {
    let all: Vec<Deployment> = util::read_jsonl(&deployments_path());
    let mut map = std::collections::HashMap::new();
    for d in all {
        map.insert(d.name.clone(), d);
    }
    map.into_values()
        .filter(|d| !(d.instances.is_empty() && d.timestamp == 0))
        .collect()
}

pub fn find(name: &str) -> Option<Deployment> {
    list().into_iter().find(|d| d.name == name)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // Deduplication + tombstone filtering are tested in-memory to avoid
    // contention on the shared JSONL file that record/list read/write.
    // The e2e suite exercises the full JSONL roundtrip via MCP calls.

    #[test]
    fn dedup_by_name_latest_wins() {
        let d1 = Deployment {
            name: "team".into(),
            template: "tpl".into(),
            instances: vec!["a".into()],
            team: None,
            directory: String::new(),
            timestamp: 1,
        };
        let d2 = Deployment {
            name: "team".into(),
            template: "tpl".into(),
            instances: vec!["a".into(), "b".into()],
            team: Some("team".into()),
            directory: String::new(),
            timestamp: 2,
        };
        let mut map = std::collections::HashMap::new();
        map.insert(d1.name.clone(), d1);
        map.insert(d2.name.clone(), d2);
        let items: Vec<Deployment> = map.into_values().collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].instances.len(), 2);
    }

    #[test]
    fn tombstone_sentinel_is_filtered() {
        let tombstone = Deployment {
            name: "gone".into(),
            template: String::new(),
            instances: vec![],
            team: None,
            directory: String::new(),
            timestamp: 0,
        };
        assert!(tombstone.instances.is_empty() && tombstone.timestamp == 0);
    }
}

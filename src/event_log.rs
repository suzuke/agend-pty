//! Event log — append-only JSONL event stream.

use crate::paths;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub ts: u64,
    #[serde(rename = "type")]
    pub event_type: String,
    pub agent: String,
    pub details: String,
}

fn events_path() -> std::path::PathBuf {
    paths::run_dir().join("events.jsonl")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn log_event(event_type: &str, agent: &str, details: &str) {
    let path = events_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let event = Event {
        ts: now_secs(),
        event_type: event_type.into(),
        agent: agent.into(),
        details: details.into(),
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        if let Ok(line) = serde_json::to_string(&event) {
            let _ = writeln!(f, "{line}");
        }
    }
}

pub fn list_events(agent_filter: Option<&str>, type_filter: Option<&str>) -> Vec<Event> {
    let path = events_path();
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return vec![],
    };
    std::io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<Event>(&line).ok())
        .filter(|e| agent_filter.map(|a| e.agent == a).unwrap_or(true))
        .filter(|e| type_filter.map(|t| e.event_type == t).unwrap_or(true))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_roundtrip() {
        let e = Event {
            ts: 123,
            event_type: "state_change".into(),
            agent: "alice".into(),
            details: "Ready".into(),
        };
        let json = serde_json::to_string(&e).unwrap();
        let restored: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.event_type, "state_change");
        assert_eq!(restored.agent, "alice");
    }
}

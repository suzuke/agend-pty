//! Event log — append-only JSONL event stream.

use crate::{paths, util};
use serde::{Deserialize, Serialize};

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

pub fn log_event(event_type: &str, agent: &str, details: &str) {
    let event = Event {
        ts: util::now_secs(),
        event_type: event_type.into(),
        agent: agent.into(),
        details: details.into(),
    };
    util::append_jsonl(&events_path(), &event);
}

pub fn list_events(agent_filter: Option<&str>, type_filter: Option<&str>) -> Vec<Event> {
    let events: Vec<Event> = util::read_jsonl(&events_path());
    events
        .into_iter()
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

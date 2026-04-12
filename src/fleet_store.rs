//! Fleet-wide shared state — decisions and task board (JSONL append-only).

use crate::{paths, util};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DECISION_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub id: u64,
    pub title: String,
    pub content: String,
    pub author: String,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub assignee: String,
    pub status: String,
    #[serde(default)]
    pub result: String,
    pub created_by: String,
    pub timestamp: u64,
}

fn decisions_path() -> std::path::PathBuf {
    paths::run_dir().join("decisions.jsonl")
}
fn tasks_path() -> std::path::PathBuf {
    paths::run_dir().join("tasks.jsonl")
}

/// Initialize counters from persisted data (call on daemon startup).
pub fn init_counters() {
    let decisions: Vec<Decision> = util::read_jsonl(&decisions_path());
    let max_d = decisions.iter().map(|d| d.id).max().unwrap_or(0);
    NEXT_DECISION_ID.store(max_d + 1, Ordering::Relaxed);

    let tasks: Vec<Task> = util::read_jsonl(&tasks_path());
    let max_t = tasks
        .iter()
        .filter_map(|t| t.id.trim_start_matches('T').parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    NEXT_TASK_ID.store(max_t + 1, Ordering::Relaxed);
}

pub fn post_decision(author: &str, title: &str, content: &str) -> Decision {
    let id = NEXT_DECISION_ID.fetch_add(1, Ordering::Relaxed);
    let d = Decision {
        id,
        title: title.into(),
        content: content.into(),
        author: author.into(),
        timestamp: util::now_secs(),
    };
    util::append_jsonl(&decisions_path(), &d);
    d
}

pub fn list_decisions() -> Vec<Decision> {
    let all: Vec<Decision> = util::read_jsonl(&decisions_path());
    let mut map = std::collections::HashMap::new();
    for d in all {
        map.insert(d.id, d);
    }
    map.into_values().collect()
}

pub fn update_decision(id: u64, title: Option<&str>, content: Option<&str>) -> Option<Decision> {
    let decisions = list_decisions();
    let mut d = decisions.into_iter().find(|d| d.id == id)?;
    if let Some(t) = title {
        d.title = t.into();
    }
    if let Some(c) = content {
        d.content = c.into();
    }
    d.timestamp = util::now_secs();
    util::append_jsonl(&decisions_path(), &d);
    Some(d)
}

// ── Teams ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Team {
    pub name: String,
    pub members: Vec<String>,
    pub timestamp: u64,
}

fn teams_path() -> std::path::PathBuf {
    paths::run_dir().join("teams.jsonl")
}

pub fn create_team(name: &str, members: &[String]) -> Team {
    let t = Team {
        name: name.into(),
        members: members.to_vec(),
        timestamp: util::now_secs(),
    };
    util::append_jsonl(&teams_path(), &t);
    t
}

pub fn list_teams() -> Vec<Team> {
    let all: Vec<Team> = util::read_jsonl(&teams_path());
    let mut map = std::collections::HashMap::new();
    for t in all {
        map.insert(t.name.clone(), t);
    }
    map.into_values().collect()
}

pub fn update_team(name: &str, members: &[String]) -> Option<Team> {
    let teams = list_teams();
    if !teams.iter().any(|t| t.name == name) {
        return None;
    }
    let t = Team {
        name: name.into(),
        members: members.to_vec(),
        timestamp: util::now_secs(),
    };
    util::append_jsonl(&teams_path(), &t);
    Some(t)
}

pub fn delete_team(name: &str) -> bool {
    let t = Team {
        name: name.into(),
        members: vec![],
        timestamp: 0,
    };
    util::append_jsonl(&teams_path(), &t);
    true
}

pub fn get_team_members(name: &str) -> Option<Vec<String>> {
    list_teams()
        .into_iter()
        .find(|t| t.name == name && !t.members.is_empty())
        .map(|t| t.members)
}

pub fn create_task(created_by: &str, title: &str, description: &str, assignee: &str) -> Task {
    let id = format!("T{}", NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed));
    let t = Task {
        id: id.clone(),
        title: title.into(),
        description: description.into(),
        assignee: assignee.into(),
        status: "open".into(),
        result: String::new(),
        created_by: created_by.into(),
        timestamp: util::now_secs(),
    };
    util::append_jsonl(&tasks_path(), &t);
    t
}

pub fn list_tasks() -> Vec<Task> {
    let all: Vec<Task> = util::read_jsonl(&tasks_path());
    let mut map = std::collections::HashMap::new();
    for t in all {
        map.insert(t.id.clone(), t);
    }
    map.into_values().collect()
}

pub fn update_task(
    id: &str,
    status: Option<&str>,
    assignee: Option<&str>,
    result: Option<&str>,
) -> Option<Task> {
    let tasks: Vec<Task> = util::read_jsonl(&tasks_path());
    let mut task = tasks.into_iter().find(|t| t.id == id)?;
    if let Some(s) = status {
        task.status = s.into();
    }
    if let Some(a) = assignee {
        task.assignee = a.into();
    }
    if let Some(r) = result {
        task.result = r.into();
    }
    task.timestamp = util::now_secs();
    util::append_jsonl(&tasks_path(), &task);
    Some(task)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_roundtrip() {
        let d = Decision {
            id: 1,
            title: "test".into(),
            content: "body".into(),
            author: "alice".into(),
            timestamp: 0,
        };
        let json = serde_json::to_string(&d).unwrap();
        let restored: Decision = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.title, "test");
    }

    #[test]
    fn task_roundtrip() {
        let t = Task {
            id: "T1".into(),
            title: "fix bug".into(),
            description: "".into(),
            assignee: "bob".into(),
            status: "open".into(),
            result: "".into(),
            created_by: "alice".into(),
            timestamp: 0,
        };
        let json = serde_json::to_string(&t).unwrap();
        let restored: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.id, "T1");
    }
}

//! Fleet-wide shared state — decisions and task board (JSONL append-only).

use crate::paths;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicU64, Ordering};

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

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn read_jsonl<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Vec<T> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return vec![],
    };
    std::io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect()
}

fn append_jsonl<T: Serialize>(path: &std::path::Path, item: &T) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        if let Ok(line) = serde_json::to_string(item) {
            let _ = writeln!(f, "{line}");
        }
    }
}

pub fn post_decision(author: &str, title: &str, content: &str) -> Decision {
    let decisions: Vec<Decision> = read_jsonl(&decisions_path());
    let id = decisions.len() as u64 + 1;
    let d = Decision {
        id,
        title: title.into(),
        content: content.into(),
        author: author.into(),
        timestamp: now_secs(),
    };
    append_jsonl(&decisions_path(), &d);
    d
}

pub fn list_decisions() -> Vec<Decision> {
    let all: Vec<Decision> = read_jsonl(&decisions_path());
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
    d.timestamp = now_secs();
    append_jsonl(&decisions_path(), &d);
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
        timestamp: now_secs(),
    };
    append_jsonl(&teams_path(), &t);
    t
}

pub fn list_teams() -> Vec<Team> {
    let all: Vec<Team> = read_jsonl(&teams_path());
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
        timestamp: now_secs(),
    };
    append_jsonl(&teams_path(), &t);
    Some(t)
}

pub fn delete_team(name: &str) -> bool {
    let t = Team {
        name: name.into(),
        members: vec![],
        timestamp: 0,
    };
    append_jsonl(&teams_path(), &t);
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
        timestamp: now_secs(),
    };
    append_jsonl(&tasks_path(), &t);
    t
}

pub fn list_tasks() -> Vec<Task> {
    let all: Vec<Task> = read_jsonl(&tasks_path());
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
    let tasks: Vec<Task> = read_jsonl(&tasks_path());
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
    task.timestamp = now_secs();
    append_jsonl(&tasks_path(), &task);
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

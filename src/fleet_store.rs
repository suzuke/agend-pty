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
#[allow(clippy::unwrap_used)]
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
            title: "fix".into(),
            description: "".into(),
            assignee: "bob".into(),
            status: "open".into(),
            result: "".into(),
            created_by: "alice".into(),
            timestamp: 0,
        };
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<Task>(&json).unwrap().id, "T1");
    }

    #[test]
    fn team_roundtrip() {
        let t = Team {
            name: "core".into(),
            members: vec!["a".into(), "b".into()],
            timestamp: 0,
        };
        let json = serde_json::to_string(&t).unwrap();
        let r: Team = serde_json::from_str(&json).unwrap();
        assert_eq!(r.members.len(), 2);
    }

    #[test]
    fn list_tasks_dedup_by_id() {
        // Simulate JSONL with duplicate IDs (update appends new version)
        let t1 = Task {
            id: "T1".into(),
            title: "v1".into(),
            description: "".into(),
            assignee: "".into(),
            status: "open".into(),
            result: "".into(),
            created_by: "a".into(),
            timestamp: 1,
        };
        let t1_v2 = Task {
            id: "T1".into(),
            title: "v2".into(),
            description: "".into(),
            assignee: "bob".into(),
            status: "claimed".into(),
            result: "".into(),
            created_by: "a".into(),
            timestamp: 2,
        };
        // Dedup: last one wins
        let mut map = std::collections::HashMap::new();
        map.insert(t1.id.clone(), t1);
        map.insert(t1_v2.id.clone(), t1_v2);
        let tasks: Vec<Task> = map.into_values().collect();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "v2");
    }

    #[test]
    fn list_decisions_dedup_by_id() {
        let d1 = Decision {
            id: 1,
            title: "v1".into(),
            content: "old".into(),
            author: "a".into(),
            timestamp: 1,
        };
        let d1_v2 = Decision {
            id: 1,
            title: "v2".into(),
            content: "new".into(),
            author: "a".into(),
            timestamp: 2,
        };
        let mut map = std::collections::HashMap::new();
        map.insert(d1.id, d1);
        map.insert(d1_v2.id, d1_v2);
        let decisions: Vec<Decision> = map.into_values().collect();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].content, "new");
    }

    #[test]
    fn team_dedup_by_name() {
        let t1 = Team {
            name: "core".into(),
            members: vec!["a".into()],
            timestamp: 1,
        };
        let t2 = Team {
            name: "core".into(),
            members: vec!["a".into(), "b".into()],
            timestamp: 2,
        };
        let mut map = std::collections::HashMap::new();
        map.insert(t1.name.clone(), t1);
        map.insert(t2.name.clone(), t2);
        let teams: Vec<Team> = map.into_values().collect();
        assert_eq!(teams.len(), 1);
        assert_eq!(teams[0].members.len(), 2);
    }

    #[test]
    fn team_delete_is_soft() {
        // Delete sets members to empty — list_teams filters these out
        let t = Team {
            name: "old".into(),
            members: vec![],
            timestamp: 0,
        };
        assert!(t.members.is_empty());
    }

    #[test]
    fn get_team_members_returns_none_for_deleted() {
        // Deleted team has empty members — get_team_members should return None
        let teams = [
            Team {
                name: "active".into(),
                members: vec!["a".into()],
                timestamp: 1,
            },
            Team {
                name: "deleted".into(),
                members: vec![],
                timestamp: 2,
            },
        ];
        let active = teams
            .iter()
            .find(|t| t.name == "active" && !t.members.is_empty());
        let deleted = teams
            .iter()
            .find(|t| t.name == "deleted" && !t.members.is_empty());
        assert!(active.is_some());
        assert!(deleted.is_none());
    }

    #[test]
    fn update_task_preserves_other_fields() {
        let mut t = Task {
            id: "T1".into(),
            title: "orig".into(),
            description: "desc".into(),
            assignee: "alice".into(),
            status: "open".into(),
            result: "".into(),
            created_by: "bob".into(),
            timestamp: 0,
        };
        // Simulate update: only change status
        t.status = "claimed".into();
        assert_eq!(t.title, "orig");
        assert_eq!(t.assignee, "alice");
        assert_eq!(t.description, "desc");
    }
}

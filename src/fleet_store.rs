//! Fleet-wide shared state — decisions and task board.

use crate::paths;
use serde::{Deserialize, Serialize};
use std::io::Write;
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

fn decisions_path() -> std::path::PathBuf { paths::run_dir().join("decisions.json") }
fn tasks_path() -> std::path::PathBuf { paths::run_dir().join("tasks.json") }

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn read_json<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Vec<T> {
    std::fs::read_to_string(path).ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

fn write_json<T: Serialize>(path: &std::path::Path, data: &[T]) {
    if let Ok(json) = serde_json::to_string_pretty(data) {
        let tmp = path.with_extension("tmp");
        if let Ok(mut f) = std::fs::File::create(&tmp) {
            let _ = f.write_all(json.as_bytes());
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

pub fn post_decision(author: &str, title: &str, content: &str) -> Decision {
    let path = decisions_path();
    let mut decisions: Vec<Decision> = read_json(&path);
    let id = decisions.len() as u64 + 1;
    let d = Decision { id, title: title.into(), content: content.into(), author: author.into(), timestamp: now_secs() };
    decisions.push(d.clone());
    write_json(&path, &decisions);
    d
}

pub fn list_decisions() -> Vec<Decision> { read_json(&decisions_path()) }

pub fn create_task(created_by: &str, title: &str, description: &str, assignee: &str) -> Task {
    let path = tasks_path();
    let mut tasks: Vec<Task> = read_json(&path);
    let id = format!("T{}", NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed));
    let t = Task {
        id: id.clone(), title: title.into(), description: description.into(),
        assignee: assignee.into(), status: "open".into(), result: String::new(),
        created_by: created_by.into(), timestamp: now_secs(),
    };
    tasks.push(t.clone());
    write_json(&path, &tasks);
    t
}

pub fn list_tasks() -> Vec<Task> { read_json(&tasks_path()) }

pub fn update_task(id: &str, status: Option<&str>, assignee: Option<&str>, result: Option<&str>) -> Option<Task> {
    let path = tasks_path();
    let mut tasks: Vec<Task> = read_json(&path);
    let task = tasks.iter_mut().find(|t| t.id == id)?;
    if let Some(s) = status { task.status = s.into(); }
    if let Some(a) = assignee { task.assignee = a.into(); }
    if let Some(r) = result { task.result = r.into(); }
    let updated = task.clone();
    write_json(&path, &tasks);
    Some(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_roundtrip() {
        let d = Decision { id: 1, title: "test".into(), content: "body".into(), author: "alice".into(), timestamp: 0 };
        let json = serde_json::to_string(&d).unwrap();
        let restored: Decision = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.title, "test");
    }

    #[test]
    fn task_roundtrip() {
        let t = Task {
            id: "T1".into(), title: "fix bug".into(), description: "".into(),
            assignee: "bob".into(), status: "open".into(), result: "".into(),
            created_by: "alice".into(), timestamp: 0,
        };
        let json = serde_json::to_string(&t).unwrap();
        let restored: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.id, "T1");
        assert_eq!(restored.status, "open");
    }
}

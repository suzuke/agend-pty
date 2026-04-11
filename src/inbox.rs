//! Inbox — per-agent message queue backed by JSONL files.
//!
//! Messages stored at ~/.agend/run/<pid>/inbox/{agent_name}.jsonl
//! One JSON object per line, append-only. POSIX small writes are atomic.

use crate::paths;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_DIRECT_INJECT_LEN: usize = 500;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxMessage {
    pub id: u64,
    pub sender: String,
    pub text: String,
    pub timestamp: u64,
}

pub struct InboxStore;

fn inbox_path(agent: &str) -> PathBuf {
    let dir = paths::run_dir().join("inbox");
    std::fs::create_dir_all(&dir).ok();
    dir.join(format!("{agent}.jsonl"))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl InboxStore {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self)
    }

    pub fn store_or_inject(&self, agent: &str, sender: &str, message: &str, _submit_key: &str) -> InjectAction {
        if message.len() <= MAX_DIRECT_INJECT_LEN {
            return InjectAction::Direct(format!(
                "[message from {sender} (reply via send_to_instance to \"{sender}\")] {message}"
            ));
        }
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let msg = InboxMessage {
            id,
            sender: sender.to_owned(),
            text: message.to_owned(),
            timestamp: now_secs(),
        };
        // Append to JSONL file
        let path = inbox_path(agent);
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            if let Ok(line) = serde_json::to_string(&msg) {
                let _ = writeln!(f, "{line}");
            }
        }
        let preview: String = message.chars().take(100).collect();
        InjectAction::Notification(format!(
            "[message from {sender}] {preview}... (full message in inbox, use inbox tool with id={id})"
        ))
    }

    pub fn get(&self, agent: &str, id: u64) -> Option<InboxMessage> {
        self.read_all(agent).into_iter().find(|m| m.id == id)
    }

    pub fn list(&self, agent: &str) -> Vec<InboxMessage> {
        self.read_all(agent)
    }

    pub fn clear(&self, agent: &str) {
        let path = inbox_path(agent);
        let tmp = path.with_extension("draining");
        if let Ok(()) = std::fs::rename(&path, &tmp) {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Atomic drain: returns all messages and clears inbox in one operation.
    pub fn drain(&self, agent: &str) -> Vec<InboxMessage> {
        let path = inbox_path(agent);
        let tmp = path.with_extension("draining");
        if std::fs::rename(&path, &tmp).is_err() {
            return vec![];
        }
        let msgs = match std::fs::File::open(&tmp) {
            Ok(f) => std::io::BufReader::new(f).lines().map_while(Result::ok)
                .filter_map(|line| serde_json::from_str(&line).ok()).collect(),
            Err(_) => vec![],
        };
        let _ = std::fs::remove_file(&tmp);
        msgs
    }

    fn read_all(&self, agent: &str) -> Vec<InboxMessage> {
        let path = inbox_path(agent);
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(_) => return vec![],
        };
        std::io::BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter_map(|line| serde_json::from_str(&line).ok())
            .collect()
    }
}

pub enum InjectAction {
    Direct(String),
    Notification(String),
}

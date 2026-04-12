//! Cron scheduler — uses `cron` crate for expression parsing + JSONL storage.

use crate::{paths, util};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    pub cron: String,
    pub target: String,
    pub message: String,
    pub enabled: bool,
    pub last_run: u64,
}

fn schedules_path() -> std::path::PathBuf {
    paths::run_dir().join("schedules.jsonl")
}

pub fn create_schedule(cron_expr: &str, target: &str, message: &str) -> Result<Schedule, String> {
    // Validate cron expression
    cron::Schedule::from_str(cron_expr).map_err(|e| format!("invalid cron: {e}"))?;
    let s = Schedule {
        id: format!("S{}", NEXT_ID.fetch_add(1, Ordering::Relaxed)),
        cron: cron_expr.into(),
        target: target.into(),
        message: message.into(),
        enabled: true,
        last_run: 0,
    };
    util::append_jsonl(&schedules_path(), &s);
    Ok(s)
}

pub fn list_schedules() -> Vec<Schedule> {
    let all: Vec<Schedule> = util::read_jsonl(&schedules_path());
    let mut map = std::collections::HashMap::new();
    for s in all {
        map.insert(s.id.clone(), s);
    }
    map.into_values().filter(|s| s.enabled).collect()
}

pub fn update_schedule(
    id: &str,
    enabled: Option<bool>,
    cron_expr: Option<&str>,
    message: Option<&str>,
) -> Option<Schedule> {
    let mut s = list_schedules().into_iter().find(|s| s.id == id)?;
    if let Some(e) = enabled {
        s.enabled = e;
    }
    if let Some(c) = cron_expr {
        s.cron = c.into();
    }
    if let Some(m) = message {
        s.message = m.into();
    }
    util::append_jsonl(&schedules_path(), &s);
    Some(s)
}

pub fn delete_schedule(id: &str) -> bool {
    if let Some(mut s) = list_schedules().into_iter().find(|s| s.id == id) {
        s.enabled = false;
        util::append_jsonl(&schedules_path(), &s);
        true
    } else {
        false
    }
}

/// Check which schedules should fire now. Returns (id, target, message) tuples.
pub fn check_due(now_epoch: u64) -> Vec<(String, String, String)> {
    let schedules = list_schedules();
    let mut due = Vec::new();
    for s in &schedules {
        if let Ok(sched) = cron::Schedule::from_str(&s.cron) {
            // Check if any scheduled time falls between last_run and now
            let after = chrono::DateTime::from_timestamp(s.last_run as i64, 0).unwrap_or_default();
            if let Some(next) = sched.after(&after).next() {
                if next.timestamp() as u64 <= now_epoch {
                    due.push((s.id.clone(), s.target.clone(), s.message.clone()));
                }
            }
        }
    }
    due
}

pub fn mark_run(id: &str) {
    if let Some(mut s) = list_schedules().into_iter().find(|s| s.id == id) {
        s.last_run = util::now_secs();
        util::append_jsonl(&schedules_path(), &s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_cron_creates() {
        assert!(create_schedule("0 * * * * *", "alice", "ping").is_ok());
    }

    #[test]
    fn invalid_cron_errors() {
        assert!(create_schedule("bad", "alice", "ping").is_err());
    }

    #[test]
    fn schedule_roundtrip() {
        let s = Schedule {
            id: "S1".into(),
            cron: "0 * * * * *".into(),
            target: "alice".into(),
            message: "ping".into(),
            enabled: true,
            last_run: 0,
        };
        let json = serde_json::to_string(&s).unwrap();
        let r: Schedule = serde_json::from_str(&json).unwrap();
        assert_eq!(r.id, "S1");
    }
}

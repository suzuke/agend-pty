//! Cron scheduler — simple cron expression matching + JSONL storage.

use crate::paths;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
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

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn append(s: &Schedule) {
    let path = schedules_path();
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).ok();
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        if let Ok(line) = serde_json::to_string(s) {
            let _ = writeln!(f, "{line}");
        }
    }
}

pub fn create_schedule(cron: &str, target: &str, message: &str) -> Schedule {
    let s = Schedule {
        id: format!("S{}", NEXT_ID.fetch_add(1, Ordering::Relaxed)),
        cron: cron.into(),
        target: target.into(),
        message: message.into(),
        enabled: true,
        last_run: 0,
    };
    append(&s);
    s
}

pub fn list_schedules() -> Vec<Schedule> {
    let all: Vec<Schedule> = {
        let path = schedules_path();
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(_) => return vec![],
        };
        std::io::BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter_map(|l| serde_json::from_str(&l).ok())
            .collect()
    };
    let mut map = std::collections::HashMap::new();
    for s in all {
        map.insert(s.id.clone(), s);
    }
    map.into_values().filter(|s| s.enabled).collect()
}

pub fn update_schedule(
    id: &str,
    enabled: Option<bool>,
    cron: Option<&str>,
    message: Option<&str>,
) -> Option<Schedule> {
    let mut s = list_schedules().into_iter().find(|s| s.id == id)?;
    if let Some(e) = enabled {
        s.enabled = e;
    }
    if let Some(c) = cron {
        s.cron = c.into();
    }
    if let Some(m) = message {
        s.message = m.into();
    }
    append(&s);
    Some(s)
}

pub fn delete_schedule(id: &str) -> bool {
    if let Some(mut s) = list_schedules().into_iter().find(|s| s.id == id) {
        s.enabled = false;
        append(&s);
        true
    } else {
        false
    }
}

/// Simple cron matching: "min hour dom mon dow" (* = any, */n = every n)
pub fn cron_matches(cron: &str, now_epoch: u64) -> bool {
    let tm = epoch_to_parts(now_epoch);
    let fields: Vec<&str> = cron.split_whitespace().collect();
    if fields.len() != 5 {
        return false;
    }
    field_matches(fields[0], tm.0) // minute
        && field_matches(fields[1], tm.1) // hour
        && field_matches(fields[2], tm.2) // day of month
        && field_matches(fields[3], tm.3) // month
        && field_matches(fields[4], tm.4) // day of week
}

fn field_matches(field: &str, value: u32) -> bool {
    if field == "*" {
        return true;
    }
    if let Some(n) = field.strip_prefix("*/") {
        return n
            .parse::<u32>()
            .map(|n| n > 0 && value % n == 0)
            .unwrap_or(false);
    }
    field.parse::<u32>().map(|v| v == value).unwrap_or(false)
}

fn epoch_to_parts(epoch: u64) -> (u32, u32, u32, u32, u32) {
    // Simple UTC time decomposition
    let secs = epoch % 86400;
    let min = (secs / 60) % 60;
    let hour = secs / 3600;
    let days = (epoch / 86400) as i64;
    let dow = ((days + 4) % 7) as u32; // 0=Sun
                                       // Approximate date (good enough for cron)
    let (y, m, d) = days_to_ymd(days);
    (min as u32, hour as u32, d, m, dow)
}

fn days_to_ymd(days: i64) -> (i32, u32, u32) {
    let mut y = 1970i32;
    let mut rem = days;
    loop {
        let ydays = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if rem < ydays {
            break;
        }
        rem -= ydays;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let mdays = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0u32;
    for md in mdays {
        if rem < md {
            break;
        }
        rem -= md;
        m += 1;
    }
    (y, m + 1, rem as u32 + 1)
}

/// Mark schedule as run (update last_run timestamp).
pub fn mark_run(id: &str) {
    if let Some(mut s) = list_schedules().into_iter().find(|s| s.id == id) {
        s.last_run = now_secs();
        append(&s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_wildcard() {
        assert!(cron_matches("* * * * *", 0));
    }

    #[test]
    fn cron_specific_minute() {
        // 2026-01-01 00:05:00 UTC = epoch 1767225900
        assert!(cron_matches("5 * * * *", 1767225900));
        assert!(!cron_matches("6 * * * *", 1767225900));
    }

    #[test]
    fn cron_every_n() {
        assert!(cron_matches("*/5 * * * *", 1767225900));
    }

    #[test]
    fn field_matches_star() {
        assert!(field_matches("*", 42));
    }
    #[test]
    fn field_matches_exact() {
        assert!(field_matches("5", 5));
        assert!(!field_matches("5", 6));
    }
    #[test]
    fn field_matches_step() {
        assert!(field_matches("*/10", 30));
        assert!(!field_matches("*/10", 31));
    }

    #[test]
    fn schedule_roundtrip() {
        let s = Schedule {
            id: "S1".into(),
            cron: "* * * * *".into(),
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

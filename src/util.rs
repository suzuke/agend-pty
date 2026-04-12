//! Shared utilities — JSONL I/O and timestamp helpers.

use serde::Serialize;
use std::io::{BufRead, Write};
use std::path::Path;

/// Lock a Mutex, logging a warning if poisoned.
pub fn lock_or_warn<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| {
        tracing::error!("mutex poisoned, recovering");
        e.into_inner()
    })
}

/// Sanitize an agent/instance name for safe use in file paths.
/// Only allows alphanumeric, hyphen, underscore. Strips leading hyphens.
pub fn sanitize_name(name: &str) -> String {
    let s: String = name
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    s.trim_start_matches('-').to_owned()
}

/// Split a command string respecting double-quoted segments.
pub fn split_command(cmd: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for ch in cmd.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ' ' if !in_quotes => {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// Atomic write: write to tmp file, then rename. Prevents partial reads.
pub fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)
}

/// Current time as seconds since UNIX epoch.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Read all JSONL lines from a file, skipping parse errors.
pub fn read_jsonl<T: serde::de::DeserializeOwned>(path: &Path) -> Vec<T> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_command() {
        assert_eq!(
            split_command("claude --model sonnet"),
            vec!["claude", "--model", "sonnet"]
        );
        assert_eq!(split_command(""), Vec::<String>::new());
        assert_eq!(
            split_command("claude \"my model\""),
            vec!["claude", "my model"]
        );
        assert_eq!(split_command("  spaces  "), vec!["spaces"]);
        assert_eq!(split_command("a \"b c\" d"), vec!["a", "b c", "d"]);
        assert_eq!(
            split_command("unmatched \"quote"),
            vec!["unmatched", "quote"]
        );
    }

    #[test]
    fn test_read_jsonl_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.jsonl");
        #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
        struct Item {
            id: u64,
            name: String,
        }
        append_jsonl(
            &path,
            &Item {
                id: 1,
                name: "alice".into(),
            },
        );
        append_jsonl(
            &path,
            &Item {
                id: 2,
                name: "bob".into(),
            },
        );
        let items: Vec<Item> = read_jsonl(&path);
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0],
            Item {
                id: 1,
                name: "alice".into()
            }
        );
        assert_eq!(
            items[1],
            Item {
                id: 2,
                name: "bob".into()
            }
        );
    }

    #[test]
    fn test_read_jsonl_nonexistent() {
        let items: Vec<serde_json::Value> = read_jsonl(std::path::Path::new("/nonexistent.jsonl"));
        assert!(items.is_empty());
    }

    #[test]
    fn test_read_jsonl_skips_bad_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mixed.jsonl");
        std::fs::write(&path, "{\"id\":1}\nnot json\n{\"id\":2}\n").unwrap();
        let items: Vec<serde_json::Value> = read_jsonl(&path);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["id"], 1);
        assert_eq!(items[1]["id"], 2);
    }

    #[test]
    fn test_append_jsonl_creates_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("deep").join("dir").join("data.jsonl");
        append_jsonl(&path, &serde_json::json!({"key": "value"}));
        assert!(path.exists());
        let items: Vec<serde_json::Value> = read_jsonl(&path);
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn test_atomic_write() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        atomic_write(&path, r#"{"key":"value"}"#).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"key":"value"}"#
        );
        // Tmp file should be cleaned up
        assert!(!tmp.path().join("config.tmp").exists());
    }

    #[test]
    fn test_atomic_write_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("data.txt");
        std::fs::write(&path, "old").unwrap();
        atomic_write(&path, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    }

    #[test]
    fn test_now_secs_reasonable() {
        let ts = now_secs();
        // Should be after 2024-01-01 (1704067200)
        assert!(ts > 1704067200);
    }

    #[test]
    fn test_sanitize_name() {
        assert_eq!(sanitize_name("alice"), "alice");
        assert_eq!(sanitize_name("my-agent_1"), "my-agent_1");
        assert_eq!(sanitize_name("../../../etc"), "etc");
        assert_eq!(sanitize_name("a/b\\c.d"), "abcd");
        assert_eq!(sanitize_name("--leading"), "leading");
        assert_eq!(sanitize_name(""), "");
    }
}

/// Append a single item as a JSONL line to a file (creates parent dirs if needed).
pub fn append_jsonl<T: Serialize>(path: &Path, item: &T) {
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!(path = %parent.display(), error = %e, "failed to create dir");
            return;
        }
    }
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(mut f) => {
            if let Ok(line) = serde_json::to_string(item) {
                if let Err(e) = writeln!(f, "{line}") {
                    tracing::warn!(path = %path.display(), error = %e, "JSONL write failed");
                }
            }
        }
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "JSONL open failed");
        }
    }
}

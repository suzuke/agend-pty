//! Shared utilities — JSONL I/O and timestamp helpers.

use serde::Serialize;
use std::io::{BufRead, Write};
use std::path::Path;

/// Sanitize an agent/instance name for safe use in file paths.
/// Only allows alphanumeric, hyphen, underscore. Strips leading hyphens.
pub fn sanitize_name(name: &str) -> String {
    let s: String = name
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    s.trim_start_matches('-').to_owned()
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

/// Append a single item as a JSONL line to a file (creates parent dirs if needed).
pub fn append_jsonl<T: Serialize>(path: &Path, item: &T) {
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

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

/// Split a command string respecting single- and double-quoted segments.
/// Matching quote characters are consumed (not part of the token).
/// Non-matching quotes inside the opposite quote style are kept literal.
pub fn split_command(cmd: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for ch in cmd.chars() {
        match ch {
            '"' | '\'' if quote.is_none() => quote = Some(ch),
            c if Some(c) == quote => quote = None,
            ' ' if quote.is_none() => {
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
#[allow(clippy::unwrap_used)]
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
        // Single-quote support (shell idiom): keep spaces inside '...' together.
        assert_eq!(
            split_command("env PS1='> ' bash --norc"),
            vec!["env", "PS1=> ", "bash", "--norc"]
        );
        assert_eq!(split_command("a 'b c' d"), vec!["a", "b c", "d"]);
        // Mixed quoting: single quotes can wrap double-quote chars literally.
        assert_eq!(
            split_command("echo 'say \"hi\"'"),
            vec!["echo", "say \"hi\""]
        );
        // Unmatched single quote behaves like unmatched double: consume to end.
        assert_eq!(split_command("x 'y z"), vec!["x", "y z"]);
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

// ── Binary framing (shared between daemon and TUI) ──────────────────────

pub const TAG_DATA: u8 = 0;
pub const TAG_RESIZE: u8 = 1;
pub const MAX_FRAME_SIZE: usize = 1_000_000;

pub fn write_frame(w: &mut impl std::io::Write, data: &[u8]) -> std::io::Result<()> {
    w.write_all(&[TAG_DATA])?;
    w.write_all(&(data.len() as u32).to_be_bytes())?;
    w.write_all(data)?;
    w.flush()
}

pub fn read_tagged_frame(r: &mut impl std::io::Read) -> std::io::Result<(u8, Vec<u8>)> {
    let mut tag = [0u8; 1];
    r.read_exact(&mut tag)?;
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok((tag[0], buf))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod framing_tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn frame_roundtrip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"hello world").unwrap();
        let mut cursor = Cursor::new(&buf);
        let (tag, data) = read_tagged_frame(&mut cursor).unwrap();
        assert_eq!(tag, TAG_DATA);
        assert_eq!(data, b"hello world");
    }

    #[test]
    fn frame_empty_data() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"").unwrap();
        let (tag, data) = read_tagged_frame(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(tag, TAG_DATA);
        assert!(data.is_empty());
    }

    #[test]
    fn frame_large_data() {
        let payload = vec![0x42u8; 65536];
        let mut buf = Vec::new();
        write_frame(&mut buf, &payload).unwrap();
        let (_, data) = read_tagged_frame(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(data.len(), 65536);
    }

    #[test]
    fn frame_oversized_rejected() {
        // Craft a frame header claiming > MAX_FRAME_SIZE
        let mut buf = vec![TAG_DATA];
        buf.extend_from_slice(&(2_000_000u32).to_be_bytes());
        let result = read_tagged_frame(&mut Cursor::new(&buf));
        assert!(result.is_err());
    }

    #[test]
    fn frame_truncated_rejected() {
        // Only tag + partial length
        let buf = vec![TAG_DATA, 0, 0];
        let result = read_tagged_frame(&mut Cursor::new(&buf));
        assert!(result.is_err());
    }

    #[test]
    fn resize_frame_roundtrip() {
        let mut buf = Vec::new();
        // Write resize frame manually
        buf.push(TAG_RESIZE);
        let data = [0u8, 120, 0, 40]; // 120x40
        buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
        buf.extend_from_slice(&data);
        let (tag, payload) = read_tagged_frame(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(tag, TAG_RESIZE);
        assert_eq!(u16::from_be_bytes([payload[0], payload[1]]), 120);
        assert_eq!(u16::from_be_bytes([payload[2], payload[3]]), 40);
    }
}

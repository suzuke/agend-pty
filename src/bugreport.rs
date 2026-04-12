//! Bug report generator — exports diagnostic info to a file.

use crate::paths;
use std::fmt::Write as FmtWrite;

/// Sensitive env var key patterns.
const SENSITIVE_KEYS: &[&str] = &[
    "API_KEY",
    "SECRET",
    "TOKEN",
    "PASSWORD",
    "PRIVATE_KEY",
    "CREDENTIALS",
];

/// Redact secrets from text: bot tokens, API keys, Bearer tokens, sensitive env vars.
fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        out.push_str(&redact_line(line));
        out.push('\n');
    }
    out
}

fn redact_line(line: &str) -> String {
    let mut s = line.to_owned();
    // KEY=value patterns (env vars)
    if let Some((key, _val)) = s.split_once('=') {
        let key_upper = key.trim().to_uppercase();
        if SENSITIVE_KEYS.iter().any(|k| key_upper.contains(k)) {
            return format!("{}=***REDACTED***", key.trim());
        }
    }
    // Telegram bot token: digits:alphanum30+
    if let Some(pos) = s.find(':') {
        let before = &s[..pos];
        let after = &s[pos + 1..];
        if before.len() >= 8
            && before.chars().all(|c| c.is_ascii_digit())
            && after.len() >= 30
            && after
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            s = s.replace(&format!("{before}:{after}"), "***REDACTED***");
        }
    }
    // Prefixed API keys / tokens (all occurrences)
    for prefix in &[
        "sk-", "key-", "anthropic-", "xoxb-", "xoxp-", "xoxa-", "ghp_", "gho_", "ghs_",
        "github_pat_",
    ] {
        while let Some(start) = s.find(prefix) {
            let rest = &s[start + prefix.len()..];
            let end = rest
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
                .unwrap_or(rest.len());
            if end >= 10 {
                let token = &s[start..start + prefix.len() + end];
                s = s.replace(token, "***REDACTED***");
            } else {
                break;
            }
        }
    }
    // Bearer tokens (all occurrences)
    while let Some(start) = s.find("Bearer ") {
        let rest = &s[start + 7..];
        let end = rest
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '-' && c != '_')
            .unwrap_or(rest.len());
        if end >= 20 {
            let token = &s[start..start + 7 + end];
            s = s.replace(token, "Bearer ***REDACTED***");
        } else {
            break;
        }
    }
    s
}

fn cmd_output(cmd: &str, args: &[&str]) -> String {
    std::process::Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_else(|| "(not available)".into())
}

pub fn run() {
    let mut report = String::new();
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let ts = chrono::Local::now().format("%Y-%m-%dT%H-%M-%S");

    writeln!(report, "=== AgEnD-PTY Bug Report ===").ok();
    writeln!(report, "Generated: {now}").ok();
    writeln!(report, "Version: {}\n", env!("CARGO_PKG_VERSION")).ok();

    // System info
    writeln!(report, "=== System ===").ok();
    writeln!(report, "OS: {}", cmd_output("uname", &["-srm"])).ok();
    writeln!(report, "Rust: {}", cmd_output("rustc", &["--version"])).ok();
    writeln!(report, "agend-pty: {}\n", env!("CARGO_PKG_VERSION")).ok();

    // Backends
    writeln!(report, "=== Backends ===").ok();
    for (name, binary) in [
        ("claude", "claude"),
        ("kiro-cli", "kiro-cli"),
        ("codex", "codex"),
        ("opencode", "opencode"),
        ("gemini", "gemini"),
        ("git", "git"),
    ] {
        match paths::which(binary) {
            Some(p) => writeln!(report, "{name}: {} ✓", p.display()).ok(),
            None => writeln!(report, "{name}: not found ✗").ok(),
        };
    }
    writeln!(report).ok();

    // Fleet config (redacted)
    writeln!(report, "=== Fleet Config ===").ok();
    let fleet_paths = [
        std::path::PathBuf::from("fleet.yaml"),
        paths::home().join("fleet.yaml"),
    ];
    let mut found_fleet = false;
    for fp in &fleet_paths {
        if let Ok(content) = std::fs::read_to_string(fp) {
            writeln!(report, "# {}", fp.display()).ok();
            write!(report, "{}", redact(&content)).ok();
            found_fleet = true;
            break;
        }
    }
    if !found_fleet {
        writeln!(report, "(no fleet.yaml found)").ok();
    }
    writeln!(report).ok();

    // Running daemons
    writeln!(report, "=== Running Daemons ===").ok();
    let daemons = paths::list_daemons();
    if daemons.is_empty() {
        writeln!(report, "(none)").ok();
    } else {
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        for d in &daemons {
            let up = epoch.saturating_sub(d.start_time);
            writeln!(
                report,
                "PID {} | fleet: {} | agents: {} | uptime: {}m",
                d.pid,
                d.fleet_config,
                d.agent_count,
                up / 60
            )
            .ok();
        }
    }
    writeln!(report).ok();

    // Socket status
    writeln!(report, "=== Socket Status ===").ok();
    if let Some(run) = paths::find_active_run_dir() {
        let api = run.join("api.sock");
        let ctrl = run.join("ctrl.sock");
        writeln!(
            report,
            "api.sock: {}",
            if api.exists() { "exists" } else { "missing" }
        )
        .ok();
        writeln!(
            report,
            "ctrl.sock: {}",
            if ctrl.exists() { "exists" } else { "missing" }
        )
        .ok();
        let agents_dir = run.join("agents");
        if let Ok(entries) = std::fs::read_dir(&agents_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                let tui = entry.path().join("tui.sock").exists();
                writeln!(report, "  {name}/tui.sock: {}", if tui { "✓" } else { "✗" }).ok();
            }
        }
    } else {
        writeln!(report, "(no active daemon)").ok();
    }
    writeln!(report).ok();

    // Recent events
    writeln!(report, "=== Recent Events (last 50) ===").ok();
    if let Some(run) = paths::find_active_run_dir() {
        let events_file = run.join("events.jsonl");
        if let Ok(content) = std::fs::read_to_string(&events_file) {
            let lines: Vec<&str> = content.lines().collect();
            let start = lines.len().saturating_sub(50);
            for line in &lines[start..] {
                writeln!(report, "{line}").ok();
            }
        } else {
            writeln!(report, "(no events)").ok();
        }
    } else {
        writeln!(report, "(no active daemon)").ok();
    }
    writeln!(report).ok();

    // Worktrees
    writeln!(report, "=== Worktrees ===").ok();
    let wt_output = cmd_output("git", &["worktree", "list"]);
    writeln!(report, "{wt_output}").ok();

    // Write to file
    let filename = format!("agend-bugreport-{ts}.txt");
    match std::fs::write(&filename, &report) {
        Ok(()) => println!("Bug report written to {filename}"),
        Err(e) => eprintln!("Error writing report: {e}"),
    }
}

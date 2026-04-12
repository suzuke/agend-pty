//! Interactive setup wizard — generates fleet.yaml from user input.

use std::io::{self, Write};
use std::path::PathBuf;

const BACKENDS: &[(&str, &str, &str)] = &[
    ("claude", "Claude Code", "claude"),
    ("kiro-cli", "Kiro CLI", "kiro-cli"),
    ("codex", "Codex", "codex"),
    ("opencode", "OpenCode", "opencode"),
    ("gemini", "Gemini", "gemini"),
];

fn prompt(msg: &str) -> String {
    print!("{msg}");
    io::stdout().flush().ok();
    let mut buf = String::new();
    if io::stdin().read_line(&mut buf).unwrap_or(0) == 0 {
        eprintln!("\nEOF — exiting.");
        std::process::exit(1);
    }
    buf.trim().to_owned()
}

fn prompt_yn(msg: &str, default_yes: bool) -> bool {
    let hint = if default_yes { "Y/n" } else { "y/N" };
    let ans = prompt(&format!("{msg} ({hint}) "));
    if ans.is_empty() {
        default_yes
    } else {
        ans.starts_with('y') || ans.starts_with('Y')
    }
}

fn prompt_nonempty(msg: &str) -> String {
    loop {
        let ans = prompt(msg);
        if !ans.is_empty() {
            return ans;
        }
        println!("  Value cannot be empty.");
    }
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var("PATH")
        .ok()?
        .split(':')
        .map(|d| PathBuf::from(d).join(name))
        .find(|p| p.exists())
}

fn verify_token(token: &str) -> Result<String, String> {
    let url = format!("https://api.telegram.org/bot{token}/getMe");
    let mut resp = isahc::get(&url).map_err(|e| format!("{e}"))?;
    use isahc::ReadResponseExt;
    let body = resp.text().map_err(|e| format!("{e}"))?;
    let j: serde_json::Value = serde_json::from_str(&body).map_err(|e| format!("{e}"))?;
    if j["ok"].as_bool() == Some(true) {
        Ok(format!(
            "@{}",
            j["result"]["username"].as_str().unwrap_or("unknown")
        ))
    } else {
        Err("invalid token".into())
    }
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        format!(
            "{}/{rest}",
            std::env::var("HOME").unwrap_or_else(|_| ".".into())
        )
    } else {
        path.to_owned()
    }
}

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| ".".into())
}

/// Check if a string looks like a Telegram bot token (digits:alphanumeric).
fn looks_like_bot_token(s: &str) -> bool {
    let Some((num, rest)) = s.split_once(':') else {
        return false;
    };
    !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) && !rest.is_empty()
}

/// Scan .env files for a bot-token-shaped value. Returns the token if found.
fn find_env_token() -> Option<(PathBuf, String)> {
    let candidates = [
        PathBuf::from(home_dir()).join(".agend").join(".env"),
        PathBuf::from(".env"),
    ];
    for path in &candidates {
        if let Ok(content) = std::fs::read_to_string(path) {
            for line in content.lines() {
                let line = line.trim();
                if line.starts_with('#') || line.is_empty() {
                    continue;
                }
                if let Some((key, val)) = line.split_once('=') {
                    let val = val.trim().trim_matches('"').trim_matches('\'');
                    if key.trim().contains("TOKEN") && looks_like_bot_token(val) {
                        return Some((path.clone(), val.to_owned()));
                    }
                }
            }
        }
    }
    None
}

/// Find existing fleet.yaml; returns (path, instance_count).
fn find_existing_fleet() -> Option<(PathBuf, usize)> {
    let candidates = [
        PathBuf::from("fleet.yaml"),
        PathBuf::from(home_dir()).join(".agend").join("fleet.yaml"),
    ];
    for path in &candidates {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(cfg) = serde_yaml::from_str::<serde_json::Value>(&content) {
                let count = cfg
                    .get("instances")
                    .and_then(|v| v.as_object())
                    .map(|m| m.len())
                    .unwrap_or(0);
                return Some((path.clone(), count));
            }
        }
    }
    None
}

pub fn run() {
    println!("AgEnD-PTY — Quick Setup\n");

    // ── Step 0: Environment scan ──
    println!("Checking environment...");

    // 0a. Existing fleet.yaml
    if let Some((path, count)) = find_existing_fleet() {
        println!(
            "  ✓ fleet.yaml found at {} ({count} instance(s) configured)",
            path.display()
        );
        if !prompt_yn("    → Overwrite?", false) {
            println!("  Aborted.");
            return;
        }
    }

    // 0b. Existing .env with bot token
    let env_token = find_env_token();
    if let Some((ref path, _)) = env_token {
        println!("  ✓ .env found at {} (bot token detected)", path.display());
        println!("    → Will use existing token");
    }

    // 0c. Scan backend binaries
    let mut available: Vec<bool> = Vec::new();
    for (_, label, binary) in BACKENDS {
        match which(binary) {
            Some(p) => {
                println!("  ✓ {label} ({binary}) in PATH ({})", p.display());
                available.push(true);
            }
            None => {
                println!("  ✗ {binary} not found");
                available.push(false);
            }
        }
    }

    // 0d. Git
    let has_git = which("git").is_some();
    if has_git {
        println!("  ✓ git in PATH");
    } else {
        println!("  ✗ git not found (worktree isolation unavailable)");
    }
    println!();

    // Check at least one backend is available
    if !available.iter().any(|&a| a) {
        eprintln!("No supported backend found in PATH. Install one first.");
        std::process::exit(1);
    }

    // ── Step 1: Backend ──
    println!("[1/6] Which AI coding agent?");
    for (i, ((_, label, _), &avail)) in BACKENDS.iter().zip(available.iter()).enumerate() {
        let mark = if avail { " " } else { "✗" };
        println!("  {mark} {}. {label}", i + 1);
    }
    let choice: usize = loop {
        if let Ok(n) = prompt("> ").parse::<usize>() {
            if n >= 1 && n <= BACKENDS.len() {
                if available[n - 1] {
                    break n;
                }
                println!("  {} is not installed.", BACKENDS[n - 1].2);
                continue;
            }
        }
        println!("  Enter 1-{}", BACKENDS.len());
    };
    let (backend_id, backend_label, _) = BACKENDS[choice - 1];
    println!("  ✓ Selected {backend_label}\n");

    // ── Step 2: Channel (optional) ──
    println!("[2/6] Telegram integration?");
    let (mut channel_token, mut channel_group): (Option<String>, Option<i64>) = (None, None);
    if prompt_yn("  Enable Telegram?", env_token.is_some()) {
        // Use existing token or ask for new one
        let token = if let Some((_, ref tok)) = env_token {
            println!("  Using token from .env");
            tok.clone()
        } else {
            prompt("  Bot token (from @BotFather): ")
        };
        print!("  Verifying... ");
        io::stdout().flush().ok();
        match verify_token(&token) {
            Ok(name) => {
                println!("✓ {name}");
                channel_token = Some(token);
            }
            Err(e) => {
                println!("✗ {e}");
                eprintln!("  Skipping Telegram.");
            }
        }
        if channel_token.is_some() {
            channel_group = prompt("  Group ID (negative number): ").parse().ok();
            if channel_group.is_none() {
                eprintln!("  Invalid group ID, skipping Telegram.");
                channel_token = None;
            }
        }
    }
    println!();

    // ── Step 3-4: Agents ──
    println!("[3/6] First agent");
    let mut agents: Vec<(String, String)> = Vec::new();
    agents.push((
        prompt_nonempty("  Name: "),
        expand_tilde(&prompt_nonempty("  Working directory: ")),
    ));
    println!("\n[4/6] Add more agents?");
    while prompt_yn("  Add another agent?", false) {
        agents.push((
            prompt_nonempty("  Name: "),
            expand_tilde(&prompt_nonempty("  Working directory: ")),
        ));
    }
    println!();

    // ── Step 5: Worktree ──
    println!("[5/6] Git worktree isolation?");
    let worktree = if has_git {
        println!("  Agents sharing the same repo will work in isolated branches.");
        prompt_yn("  Enable worktree isolation?", true)
    } else {
        println!("  Skipped (git not found).");
        false
    };
    println!();

    // ── Step 6: Generate fleet.yaml ──
    println!("[6/6] Generating fleet.yaml...\n");
    let mut fleet = serde_json::json!({
        "defaults": { "backend": backend_id, "worktree": worktree }
    });
    if let (Some(_), Some(gid)) = (&channel_token, channel_group) {
        fleet["channel"] = serde_json::json!({
            "bot_token_env": "TELEGRAM_BOT_TOKEN",
            "group_id": gid
        });
    }
    let mut instances = serde_json::Map::new();
    for (name, wd) in &agents {
        let mut inst = serde_json::json!({"working_directory": wd});
        if backend_id == "claude" {
            inst["skip_permissions"] = serde_json::json!(true);
        }
        instances.insert(name.clone(), inst);
    }
    fleet["instances"] = serde_json::Value::Object(instances);
    let yaml = serde_yaml::to_string(&fleet).unwrap_or_default();

    let out_path = PathBuf::from(home_dir()).join(".agend").join("fleet.yaml");
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Err(e) = std::fs::write(&out_path, &yaml) {
        eprintln!("  Error writing {}: {e}", out_path.display());
        std::process::exit(1);
    }

    println!("✓ fleet.yaml written to {}\n", out_path.display());
    println!("{yaml}");
    println!("Next steps:");
    if let Some(ref tok) = channel_token {
        if env_token.is_none() {
            println!("  export TELEGRAM_BOT_TOKEN={tok}");
        }
    }
    println!("  agend-pty daemon");
    if let Some((name, _)) = agents.first() {
        println!("  agend-pty attach {name}");
    }
}

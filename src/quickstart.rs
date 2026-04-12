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
    io::stdin().read_line(&mut buf).unwrap_or_default();
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

pub fn run() {
    println!("AgEnD-PTY — Quick Setup\n");

    // Step 1: Backend
    println!("[1/6] Which AI coding agent?");
    for (i, (_, label, _)) in BACKENDS.iter().enumerate() {
        println!("  {}. {}", i + 1, label);
    }
    let choice: usize = loop {
        if let Ok(n) = prompt("> ").parse::<usize>() {
            if n >= 1 && n <= BACKENDS.len() {
                break n;
            }
        }
        println!("  Enter 1-{}", BACKENDS.len());
    };
    let (backend_id, backend_label, binary) = BACKENDS[choice - 1];
    match which(binary) {
        Some(p) => println!("  ✓ {} found ({})\n", backend_label, p.display()),
        None => {
            eprintln!("  ✗ {binary} not found in PATH. Install it first.");
            std::process::exit(1);
        }
    }

    // Step 2: Channel (optional)
    println!("[2/6] Telegram integration?");
    let (mut channel_token, mut channel_group): (Option<String>, Option<i64>) = (None, None);
    if prompt_yn("  Enable Telegram?", false) {
        let token = prompt("  Bot token (from @BotFather): ");
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

    // Step 3-4: Agents
    println!("[3/6] First agent");
    let mut agents: Vec<(String, String)> = Vec::new();
    agents.push((
        prompt("  Name: "),
        expand_tilde(&prompt("  Working directory: ")),
    ));
    println!("\n[4/6] Add more agents?");
    while prompt_yn("  Add another agent?", false) {
        agents.push((
            prompt("  Name: "),
            expand_tilde(&prompt("  Working directory: ")),
        ));
    }
    println!();

    // Step 5: Worktree
    println!("[5/6] Git worktree isolation?");
    println!("  Agents sharing the same repo will work in isolated branches.");
    let worktree = prompt_yn("  Enable worktree isolation?", true);
    println!();

    // Step 6: Generate fleet.yaml
    println!("[6/6] Generating fleet.yaml...\n");
    let mut yaml = format!("defaults:\n  backend: {backend_id}\n  worktree: {worktree}\n");
    if let (Some(_), Some(gid)) = (&channel_token, channel_group) {
        yaml.push_str(&format!(
            "\nchannel:\n  bot_token_env: TELEGRAM_BOT_TOKEN\n  group_id: {gid}\n"
        ));
    }
    yaml.push_str("\ninstances:\n");
    for (name, wd) in &agents {
        yaml.push_str(&format!("  {name}:\n    working_directory: {wd}\n"));
        if backend_id == "claude" {
            yaml.push_str("    skip_permissions: true\n");
        }
    }

    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let out_path = PathBuf::from(&home).join(".agend").join("fleet.yaml");
    if out_path.exists()
        && !prompt_yn(
            &format!("  {} exists. Overwrite?", out_path.display()),
            false,
        )
    {
        println!("  Aborted.");
        return;
    }
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
        println!("  export TELEGRAM_BOT_TOKEN={tok}");
    }
    println!("  agend-pty daemon");
    if let Some((name, _)) = agents.first() {
        println!("  agend-pty attach {name}");
    }
}

use crate::{config, git, paths};

pub fn run() {
    let mut ok = 0;
    let mut warn = 0;
    let mut fail = 0;

    println!("AgEnD-PTY Health Check\n");

    // 1. Home directory
    let home = paths::home();
    if home.exists() {
        println!("✓ agend home: {}", home.display());
        ok += 1;
    } else {
        println!(
            "✗ agend home not found: {} (run agend-pty daemon to create)",
            home.display()
        );
        fail += 1;
    }

    // 2. fleet.yaml — syntax validation
    let fleet_cfg = match config::FleetConfig::find_and_load() {
        Ok(cfg) => {
            println!("✓ fleet.yaml parsed ({} instances)", cfg.instances.len());
            ok += 1;
            Some(cfg)
        }
        Err(e) => {
            println!("✗ fleet.yaml: {e}");
            fail += 1;
            None
        }
    };

    // 3. Backend binaries (from fleet config or all supported)
    if let Some(ref cfg) = fleet_cfg {
        for (name, ic) in &cfg.instances {
            let backend = ic.backend_or(&cfg.defaults);
            let bin = backend.split_whitespace().next().unwrap_or(backend);
            if which(bin) {
                println!("✓ {name}: {bin} found");
                ok += 1;
            } else {
                println!("✗ {name}: {bin} not found in PATH");
                fail += 1;
            }
        }
    } else {
        // No fleet config — check common backends
        for (label, bin) in [("claude", "claude"), ("git", "git")] {
            if which(bin) {
                println!("✓ {label} found");
                ok += 1;
            } else {
                println!("✗ {label} not found");
                fail += 1;
            }
        }
    }

    // 4. Git
    if which("git") {
        println!("✓ git found");
        ok += 1;
    } else {
        println!("✗ git not found (worktree isolation unavailable)");
        fail += 1;
    }

    // 5. MCP binary
    let daemon_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|par| par.to_path_buf()))
        .unwrap_or_default();
    let mcp_bin = daemon_dir.join("agend-mcp");
    if mcp_bin.exists() {
        println!("✓ agend-mcp found");
        ok += 1;
    } else {
        println!("✗ agend-mcp not found. Build with: cargo build");
        fail += 1;
    }

    // 6. Telegram token validation
    if let Some(ref cfg) = fleet_cfg {
        if let Some(ch) = &cfg.channel {
            let token_env = ch.bot_token_env.as_deref().unwrap_or("TELEGRAM_BOT_TOKEN");
            match std::env::var(token_env) {
                Ok(token) => {
                    print!("  Verifying {token_env}... ");
                    match verify_telegram(&token) {
                        Ok(name) => {
                            println!("✓ {name}");
                            ok += 1;
                        }
                        Err(e) => {
                            println!("✗ {e}");
                            fail += 1;
                        }
                    }
                }
                Err(_) => {
                    println!("✗ Telegram: {token_env} not set");
                    warn += 1;
                }
            }
        }
    }

    // 7. Daemon status
    let daemons = paths::list_daemons();
    if daemons.is_empty() {
        println!("⚠ no daemon running");
        warn += 1;
    } else {
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        for d in &daemons {
            let up = epoch.saturating_sub(d.start_time);
            println!(
                "✓ daemon pid {} | {} | {} agents | uptime {}m",
                d.pid,
                d.fleet_config,
                d.agent_count,
                up / 60
            );
            ok += 1;
        }

        // Agent sockets
        if let Some(run) = paths::find_active_run_dir() {
            let agents_dir = run.join("agents");
            if let Ok(entries) = std::fs::read_dir(&agents_dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let tui = entry.path().join("tui.sock").exists();
                    if tui {
                        println!("  ✓ {name}: tui.sock ready");
                        ok += 1;
                    } else {
                        println!("  ✗ {name}: tui.sock missing");
                        warn += 1;
                    }
                }
            }
        }
    }

    // 8. Residual worktrees
    let cwd = std::env::current_dir().unwrap_or_default();
    if git::is_git_repo(&cwd) {
        let output = std::process::Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        let wt_count = output.matches("worktree ").count().saturating_sub(1); // exclude main
        if wt_count > 0 {
            println!("⚠ {wt_count} git worktree(s) found (use `agend-pty cleanup` to remove)");
            warn += 1;
        } else {
            println!("✓ no residual worktrees");
            ok += 1;
        }
    }

    // 9. API key env vars
    for var in ["ANTHROPIC_API_KEY", "GOOGLE_API_KEY", "OPENAI_API_KEY"] {
        if std::env::var(var).is_ok() {
            println!("✓ {var} set");
            ok += 1;
        }
    }

    println!("\n{ok} ok, {warn} warnings, {fail} errors");
    if fail > 0 {
        std::process::exit(1);
    }
}

fn which(name: &str) -> bool {
    paths::which(name).is_some()
}

fn verify_telegram(token: &str) -> Result<String, String> {
    let url = format!("https://api.telegram.org/bot{token}/getMe");
    let mut resp = isahc::get(&url).map_err(|e| format!("HTTP error: {e}"))?;
    use isahc::ReadResponseExt;
    let body = resp.text().map_err(|e| format!("read error: {e}"))?;
    let j: serde_json::Value = serde_json::from_str(&body).map_err(|e| format!("parse: {e}"))?;
    if j["ok"].as_bool() == Some(true) {
        Ok(format!(
            "@{}",
            j["result"]["username"].as_str().unwrap_or("unknown")
        ))
    } else {
        Err("invalid token".into())
    }
}

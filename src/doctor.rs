
use crate::{config, paths};
use std::path::Path;

pub fn run() {
    let mut ok = 0;
    let mut warn = 0;
    let mut fail = 0;

    // 1. Home directory
    let home = paths::home();
    if home.exists() {
        println!("✅ {} exists", home.display()); ok += 1;
    } else {
        println!("❌ {} not found (run agend-daemon to create)", home.display()); fail += 1;
    }

    // 2. fleet.yaml
    match config::FleetConfig::find_and_load() {
        Ok(cfg) => {
            println!("✅ fleet.yaml parsed ({} instances)", cfg.instances.len());
            ok += 1;

            // 3. Backend binaries
            for (name, ic) in &cfg.instances {
                let backend = ic.backend_or(&cfg.defaults);
                let bin = backend.split_whitespace().next().unwrap_or(backend);
                if which(bin) {
                    println!("✅ {name}: {bin} found"); ok += 1;
                } else {
                    println!("❌ {name}: {bin} not found in PATH"); fail += 1;
                }
            }

            // 4. Telegram config
            if let Some(ch) = &cfg.channel {
                let token_env = ch.bot_token_env.as_deref().unwrap_or("TELEGRAM_BOT_TOKEN");
                if std::env::var(token_env).is_ok() {
                    println!("✅ Telegram: {token_env} set"); ok += 1;
                } else {
                    println!("⚠️  Telegram: {token_env} not set"); warn += 1;
                }
            }
        }
        Err(e) => {
            println!("❌ fleet.yaml: {e}"); fail += 1;
        }
    }

    // 5. Bridge binary
    let daemon_dir = std::env::current_exe()
        .map(|p| p.parent().unwrap().to_path_buf())
        .unwrap_or_default();
    let bridge = daemon_dir.join("agend-mcp-bridge");
    if bridge.exists() {
        println!("✅ agend-mcp-bridge found at {}", bridge.display()); ok += 1;
    } else {
        println!("❌ agend-mcp-bridge not found (should be next to agend-daemon)"); fail += 1;
    }

    // 6. Active daemon
    if let Some(run) = paths::find_active_run_dir() {
        let pid = run.file_name().and_then(|f| f.to_str()).unwrap_or("?");
        println!("✅ daemon running (pid {pid})"); ok += 1;

        // Check agent sockets
        let agents_dir = run.join("agents");
        if let Ok(entries) = std::fs::read_dir(&agents_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                let tui = entry.path().join("tui.sock").exists();
                let mcp = entry.path().join("mcp.sock").exists();
                let status = match (tui, mcp) {
                    (true, true) => { ok += 1; "✅" }
                    (true, false) => { warn += 1; "⚠️ " }
                    _ => { fail += 1; "❌" }
                };
                println!("{status} {name}: tui.sock {} mcp.sock {}",
                    if tui { "✓" } else { "✗" },
                    if mcp { "✓" } else { "✗" });
            }
        }
    } else {
        println!("⚠️  no daemon running"); warn += 1;
    }

    // 7. API key env vars
    for var in ["ANTHROPIC_API_KEY", "GOOGLE_API_KEY", "OPENAI_API_KEY"] {
        if std::env::var(var).is_ok() {
            println!("✅ {var} set"); ok += 1;
        }
    }

    println!("\n{ok} ok, {warn} warnings, {fail} errors");
    if fail > 0 { std::process::exit(1); }
}

fn which(name: &str) -> bool {
    std::env::var("PATH").unwrap_or_default()
        .split(':')
        .any(|dir| Path::new(dir).join(name).exists())
}

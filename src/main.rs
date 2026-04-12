//! agend-pty — single binary with subcommands.

use agend_pty_poc::{bugreport, config, demo, doctor, features, git, paths, quickstart};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let bin_name = std::path::Path::new(&args[0])
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("agend-pty");

    // Support symlink/hardlink aliases: agend-daemon → daemon, agend-tui → attach
    let cmd = match bin_name {
        "agend-daemon" => "daemon",
        "agend-tui" => "attach",
        _ => args.get(1).map(|s| s.as_str()).unwrap_or("help"),
    };

    let sub_args: Vec<String> = if bin_name.starts_with("agend-") && bin_name != "agend-pty" {
        args[1..].to_vec() // binary name IS the command
    } else {
        args.get(2..).unwrap_or_default().to_vec()
    };

    match cmd {
        "daemon" | "start" => {
            // Exec the daemon binary (same directory)
            let bin = exe_dir().join("agend-daemon");
            exec_with_args(&bin, &sub_args);
        }
        "attach" | "a" => {
            let bin = exe_dir().join("agend-tui");
            exec_with_args(&bin, &sub_args);
        }
        "quickstart" | "init" | "setup" => {
            quickstart::run();
        }
        "demo" => {
            demo::run();
        }
        "bugreport" | "bug" => {
            bugreport::run();
        }
        "doctor" | "doc" => {
            doctor::run();
        }
        "dry-run" | "dryrun" => match config::FleetConfig::find_and_load() {
            Ok(cfg) => features::dry_run(&cfg),
            Err(e) => {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        },
        "snapshot" => {
            let output = sub_args
                .iter()
                .position(|s| s == "--output" || s == "-o")
                .and_then(|i| sub_args.get(i + 1))
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| "fleet-snapshot.json".into());
            if let Err(e) = features::snapshot(None, &output) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        "restore" => {
            let input = sub_args
                .iter()
                .position(|s| s == "--input" || s == "-i")
                .and_then(|i| sub_args.get(i + 1))
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| "fleet-snapshot.json".into());
            if let Err(e) = features::restore(&input) {
                eprintln!("Error: {e}\nUsage: agend-pty restore -i <snapshot.json>");
                std::process::exit(1);
            }
        }
        "--shutdown" | "shutdown" | "stop" => {
            if let Some(run) = paths::find_active_run_dir() {
                let ctrl = run.join("ctrl.sock");
                match std::os::unix::net::UnixStream::connect(&ctrl) {
                    Ok(mut s) => {
                        use std::io::Write;
                        let _ = s.write_all(b"shutdown");
                        println!("Shutdown signal sent.");
                    }
                    Err(e) => eprintln!("Cannot connect to daemon: {e}"),
                }
            } else {
                eprintln!("No running daemon found. Start with: agend-pty daemon");
                std::process::exit(1);
            }
        }
        "list" | "ls" => {
            let agents = paths::list_agents();
            if agents.is_empty() {
                println!("No running agents.");
            } else {
                for a in &agents {
                    println!("  {a}");
                }
            }
        }
        "status" => {
            let live = sub_args.iter().any(|s| s == "--live" || s == "-l");
            if live {
                status_live();
            } else {
                let daemons = paths::list_daemons();
                if daemons.is_empty() {
                    println!("No running daemons.");
                } else {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    for d in &daemons {
                        let uptime = now.saturating_sub(d.start_time);
                        let h = uptime / 3600;
                        let m = (uptime % 3600) / 60;
                        println!(
                            "  PID {} | fleet: {} | agents: {} | uptime: {}h{}m",
                            d.pid, d.fleet_config, d.agent_count, h, m
                        );
                    }
                }
            }
        }
        "inject" => {
            let agent = sub_args.first().map(|s| s.as_str()).unwrap_or("");
            let msg = sub_args.get(1..).unwrap_or_default().join(" ");
            if agent.is_empty() || msg.is_empty() {
                eprintln!("Usage: agend-pty inject <agent> <message>");
                std::process::exit(1);
            }
            if let Some(run) = paths::find_active_run_dir() {
                match std::os::unix::net::UnixStream::connect(run.join("api.sock")) {
                    Ok(mut s) => {
                        use std::io::{BufRead, BufReader, Write};
                        let req = serde_json::json!({"method":"inject","params":{"instance":agent,"message":msg,"sender":"cli"}});
                        writeln!(s, "{}", req).ok();
                        s.flush().ok();
                        let mut line = String::new();
                        BufReader::new(s).read_line(&mut line).ok();
                        println!("{}", line.trim());
                    }
                    Err(e) => eprintln!("Cannot connect to API: {e}"),
                }
            } else {
                eprintln!("No running daemon found. Start with: agend-pty daemon");
                std::process::exit(1);
            }
        }
        "logs" | "log" => {
            let agent = sub_args.first().map(|s| s.as_str()).unwrap_or("");
            if agent.is_empty() {
                eprintln!("Usage: agend-pty logs <agent> [--follow]");
                std::process::exit(1);
            }
            let follow = sub_args.iter().any(|s| s == "--follow" || s == "-f");
            logs_stream(agent, follow);
        }
        "cleanup" => {
            let cwd = std::env::current_dir().unwrap_or_default();
            if git::is_git_repo(&cwd) {
                let n = git::cleanup_worktrees(&cwd);
                println!("Cleaned up {n} worktree(s).");
            } else {
                println!("Not a git repo.");
            }
        }
        "help" | "--help" | "-h" => print_help(),
        "--version" | "-V" => println!("agend-pty {}", env!("CARGO_PKG_VERSION")),
        _ => {
            eprintln!("Unknown command: {cmd}");
            print_help();
            std::process::exit(1);
        }
    }
}

fn print_help() {
    println!("agend-pty — AI agent fleet manager\n");
    println!("USAGE:");
    println!("    agend-pty <COMMAND>\n");
    println!("COMMANDS:");
    println!("    quickstart             Interactive setup wizard");
    println!("    demo                   Run demo with mock agents (no API key)");
    println!("    daemon [name:cmd ...]  Start the daemon (manages agents)");
    println!("    attach [agent]         Connect TUI to a running agent");
    println!("    status [--live]         Show fleet status (--live for dashboard)");
    println!("    list                   List agents in current fleet");
    println!("    inject <agent> <msg>   Send a message to an agent");
    println!("    logs <agent> [-f]      Stream agent output (read-only)");
    println!("    dry-run                Validate fleet.yaml without starting agents");
    println!("    snapshot [-o file]     Save fleet state to JSON");
    println!("    restore [-i file]      Restore fleet from snapshot");
    println!("    cleanup                Remove leftover git worktrees");
    println!("    bugreport              Export diagnostic info to file");
    println!("    doctor                 Check system health");
    println!("    shutdown               Stop a running daemon\n");
    println!("OPTIONS:");
    println!("    -h, --help             Print help");
    println!("    -V, --version          Print version");
}

fn logs_stream(agent: &str, follow: bool) {
    let sock = match paths::find_agent_tui_socket(agent) {
        Some(s) => s,
        None => {
            eprintln!("Agent '{agent}' not found. Start with: agend-pty daemon");
            std::process::exit(1);
        }
    };
    let mut stream = match std::os::unix::net::UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Cannot connect to {agent}: {e}");
            std::process::exit(1);
        }
    };
    use std::io::{Read, Write};
    // Read frames: tag(1) + len(4 BE) + payload
    let read_frame = |r: &mut std::os::unix::net::UnixStream| -> std::io::Result<Vec<u8>> {
        let mut hdr = [0u8; 5];
        r.read_exact(&mut hdr)?;
        let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf)?;
        Ok(buf)
    };
    // First frame = screen dump (current state)
    if let Ok(data) = read_frame(&mut stream) {
        if std::io::stdout().write_all(&data).is_err() {
            return;
        }
        let _ = std::io::stdout().flush();
    }
    if !follow {
        println!();
        return;
    }
    // Stream subsequent frames
    while let Ok(data) = read_frame(&mut stream) {
        if std::io::stdout().write_all(&data).is_err() {
            break;
        }
        let _ = std::io::stdout().flush();
    }
}

fn exe_dir() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|par| par.to_path_buf()))
        .unwrap_or_default()
}

fn status_live() {
    let run = match paths::find_active_run_dir() {
        Some(r) => r,
        None => {
            eprintln!("No running daemon found. Start with: agend-pty daemon");
            std::process::exit(1);
        }
    };
    let sock = run.join("api.sock");
    loop {
        // Clear screen + home
        print!("\x1b[2J\x1b[H");
        let resp = {
            use std::io::{BufRead, Write};
            std::os::unix::net::UnixStream::connect(&sock)
                .and_then(|mut s| {
                    s.set_read_timeout(Some(std::time::Duration::from_secs(2)))
                        .ok();
                    writeln!(s, r#"{{"method":"status","params":{{}}}}"#)?;
                    s.flush()?;
                    let mut line = String::new();
                    std::io::BufReader::new(s).read_line(&mut line)?;
                    Ok(line)
                })
                .ok()
                .and_then(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        };
        let agents = resp
            .as_ref()
            .and_then(|r| r["result"]["agents"].as_array())
            .cloned()
            .unwrap_or_default();
        let daemon = paths::list_daemons().into_iter().next();
        let uptime = daemon
            .as_ref()
            .map(|d| {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let s = now.saturating_sub(d.start_time);
                format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
            })
            .unwrap_or_else(|| "?".into());

        println!("┌─ agend-pty fleet ──────────────────────────────┐");
        let mut healthy = 0u32;
        for a in &agents {
            let name = a["name"].as_str().unwrap_or("?");
            let state = a["state"].as_str().unwrap_or("?");
            let icon = match state {
                "Ready" | "Idle" => "●",
                "Busy" => "◐",
                "Starting" | "Restarting" => "○",
                _ => "✗",
            };
            let health = a["health"].as_str().unwrap_or("?");
            if health == "Healthy" {
                healthy += 1;
            }
            println!(
                "│ {:<8} {icon} {:<12} │ {:<8} │ agend/{:<8} │",
                name, state, health, name
            );
        }
        let total = agents.len() as u32;
        println!("├────────────────────────────────────────────────┤");
        println!(
            "│ Health: {healthy}/{total} healthy │ Uptime: {:<19} │",
            uptime
        );
        println!("└────────────────────────────────────────────────┘");
        println!("\nPress Ctrl+C to exit. Refreshing every 2s...");
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

fn exec_with_args(bin: &std::path::Path, args: &[String]) {
    let status = std::process::Command::new(bin).args(args).status();
    match status {
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("Failed to exec {}: {e}", bin.display());
            std::process::exit(1);
        }
    }
}

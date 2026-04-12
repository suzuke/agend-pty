#![allow(dead_code, unused_imports)]
//! agend-pty — single binary with subcommands.
//!
//! Usage:
//!   agend-pty daemon [name:command ...]   Start the daemon
//!   agend-pty attach [agent-name]         Attach to agent terminal
//!   agend-pty doctor                      Health check
//!   agend-pty list                        List running agents
//!   agend-pty inject <agent> <message>    Inject message to agent
//!   agend-pty shutdown                    Stop running daemon

#[path = "config.rs"]
mod config;
#[path = "doctor.rs"]
mod doctor;
#[path = "features.rs"]
mod features;
#[path = "git.rs"]
mod git;
#[path = "instructions.rs"]
mod instructions;
#[path = "paths.rs"]
mod paths;
#[path = "quickstart.rs"]
mod quickstart;

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
                eprintln!("Error: {e}");
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
            }
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
    println!("    daemon [name:cmd ...]  Start the daemon (manages agents)");
    println!("    attach [agent]         Connect TUI to a running agent");
    println!("    status                 Show running daemons and agents");
    println!("    list                   List agents in current fleet");
    println!("    inject <agent> <msg>   Send a message to an agent");
    println!("    dry-run                Validate fleet.yaml without starting agents");
    println!("    snapshot [-o file]     Save fleet state to JSON");
    println!("    restore [-i file]      Restore fleet from snapshot");
    println!("    cleanup                Remove leftover git worktrees");
    println!("    doctor                 Check system health");
    println!("    shutdown               Stop a running daemon\n");
    println!("OPTIONS:");
    println!("    -h, --help             Print help");
    println!("    -V, --version          Print version");
}

fn exe_dir() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|par| par.to_path_buf()))
        .unwrap_or_default()
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

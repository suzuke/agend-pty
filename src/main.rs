//! agend-pty — single binary with subcommands.
//!
//! Usage:
//!   agend-pty daemon [name:command ...]   Start the daemon
//!   agend-pty attach [agent-name]         Attach to agent terminal
//!   agend-pty doctor                      Health check
//!   agend-pty list                        List running agents
//!   agend-pty inject <agent> <message>    Inject message to agent
//!   agend-pty shutdown                    Stop running daemon

#[path = "paths.rs"]
mod paths;
#[path = "doctor.rs"]
mod doctor;
#[path = "config.rs"]
mod config;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let bin_name = std::path::Path::new(&args[0]).file_name()
        .and_then(|f| f.to_str()).unwrap_or("agend-pty");

    // Support symlink/hardlink aliases: agend-daemon → daemon, agend-tui → attach
    let cmd = match bin_name {
        "agend-daemon" => "daemon",
        "agend-tui" => "attach",
        "agend-mcp-bridge" => "mcp-bridge",
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
        "mcp-bridge" => {
            let bin = exe_dir().join("agend-mcp-bridge");
            exec_with_args(&bin, &sub_args);
        }
        "doctor" | "doc" => {
            doctor::run();
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
                eprintln!("No running daemon found.");
            }
        }
        "list" | "ls" => {
            let agents = paths::list_agents();
            if agents.is_empty() {
                println!("No running agents.");
            } else {
                for a in &agents { println!("  {a}"); }
            }
        }
        "status" => {
            let daemons = paths::list_daemons();
            if daemons.is_empty() {
                println!("No running daemons.");
            } else {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                for d in &daemons {
                    let uptime = now.saturating_sub(d.start_time);
                    let h = uptime / 3600; let m = (uptime % 3600) / 60;
                    println!("  PID {} | fleet: {} | agents: {} | uptime: {}h{}m",
                        d.pid, d.fleet_config, d.agent_count, h, m);
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
                eprintln!("No running daemon found.");
            }
        }
        "help" | "--help" | "-h" => print_help(),
        _ => {
            eprintln!("Unknown command: {cmd}");
            print_help();
            std::process::exit(1);
        }
    }
}

fn print_help() {
    println!("agend-pty — AI agent fleet orchestrator\n");
    println!("Commands:");
    println!("  daemon [name:cmd ...]  Start daemon (reads fleet.yaml if no args)");
    println!("  attach [agent]         Attach to agent terminal (Ctrl+B d to detach)");
    println!("  doctor                 Health check");
    println!("  list                   List running agents");
    println!("  status                 List running daemons");
    println!("  inject <agent> <msg>   Inject message to agent");
    println!("  shutdown               Stop running daemon");
}

fn exe_dir() -> std::path::PathBuf {
    std::env::current_exe()
        .map(|p| p.parent().unwrap().to_path_buf())
        .unwrap_or_default()
}

fn exec_with_args(bin: &std::path::Path, args: &[String]) {
    let status = std::process::Command::new(bin)
        .args(args)
        .status();
    match status {
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("Failed to exec {}: {e}", bin.display());
            std::process::exit(1);
        }
    }
}

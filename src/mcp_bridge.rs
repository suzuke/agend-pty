//! agend-mcp-bridge: NDJSON passthrough between stdio and daemon MCP socket.
//!
//! Claude sends NDJSON on stdin → forward to daemon socket.
//! Daemon sends NDJSON on socket → forward to stdout.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

#[path = "paths.rs"]
mod paths;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Parse: --socket <path> (explicit) or <agent-name> (discovery)
    let sock_path = if args.len() >= 2 && args[0] == "--socket" {
        std::path::PathBuf::from(&args[1])
    } else if !args.is_empty() {
        let agent = &args[0];
        let mut attempts = 0;
        loop {
            if let Some(p) = paths::find_agent_mcp_socket(agent) { break p; }
            attempts += 1;
            if attempts > 30 {
                eprintln!("[bridge] MCP socket for '{agent}' not found");
                std::process::exit(1);
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    } else {
        eprintln!("Usage: agend-mcp-bridge --socket <path> | <agent-name>");
        std::process::exit(1);
    };

    let stream = UnixStream::connect(&sock_path).unwrap_or_else(|e| {
        eprintln!("[bridge] connect to {}: {e}", sock_path.display());
        std::process::exit(1);
    });

    let mut sock_writer = stream.try_clone().expect("clone");
    let sock_reader = stream;

    // Thread: daemon socket (NDJSON) → stdout (NDJSON)
    let mut stdout = std::io::stdout();
    std::thread::Builder::new()
        .name("sock_to_stdout".into())
        .spawn(move || {
            let reader = BufReader::new(sock_reader);
            for line in reader.lines().flatten() {
                if line.trim().is_empty() { continue; }
                if writeln!(stdout, "{}", line.trim()).is_err() { return; }
                let _ = stdout.flush();
            }
        })
        .unwrap();

    // Main thread: stdin (NDJSON) → daemon socket (NDJSON)
    let stdin = std::io::stdin();
    let reader = BufReader::new(stdin.lock());
    for line in reader.lines().flatten() {
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }
        if writeln!(sock_writer, "{trimmed}").is_err() { break; }
        let _ = sock_writer.flush();
    }
}

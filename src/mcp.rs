#![allow(dead_code)]
//! MCP server — identity-injecting bridge to daemon API socket.
//! Spawned by CLI agents as their MCP server process.
//! Instance identity via AGEND_INSTANCE_NAME env var.
//!
//! Forwards all MCP JSON-RPC to daemon's API socket with `_instance`
//! field injected. Daemon handles protocol natively.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

#[path = "paths.rs"]
mod paths;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Parse --socket <path> for explicit API socket override
    let explicit_socket = if args.len() >= 2 && args[0] == "--socket" {
        Some(std::path::PathBuf::from(&args[1]))
    } else {
        None
    };

    let instance = std::env::var("AGEND_INSTANCE_NAME").unwrap_or_else(|_| {
        let positional = if explicit_socket.is_some() {
            args.get(2)
        } else {
            args.first()
        };
        positional.cloned().unwrap_or_else(|| {
            eprintln!("Usage: agend-mcp [--socket <path>] <instance-name>");
            eprintln!("  or set AGEND_INSTANCE_NAME env var");
            std::process::exit(1);
        })
    });

    // Find API socket (retry for daemon startup)
    let api_sock = if let Some(sock) = explicit_socket {
        if !sock.exists() {
            eprintln!("[mcp] socket not found: {}", sock.display());
            std::process::exit(1);
        }
        sock
    } else {
        let mut attempts = 0;
        loop {
            if let Some(run) = paths::find_active_run_dir() {
                let sock = run.join("api.sock");
                if sock.exists() {
                    break sock;
                }
            }
            attempts += 1;
            if attempts > 50 {
                eprintln!(
                    "[mcp] no daemon API socket found after 5s. Start with: agend-pty daemon"
                );
                std::process::exit(1);
            }
            if attempts % 10 == 0 {
                eprintln!("[mcp] waiting for daemon API socket...");
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    };

    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut stdout = std::io::stdout();
    let mut line = String::new();

    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let mut req: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Skip notifications (no id → no response expected)
        let id = match req.get("id") {
            Some(id) => id.clone(),
            None => continue,
        };

        // Inject instance identity so daemon knows who's calling
        req["_instance"] = serde_json::json!(instance);

        // Forward to daemon API socket (which handles MCP JSON-RPC natively)
        let resp = match forward_jsonrpc(&api_sock, &req) {
            Ok(r) => r,
            Err(e) => serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32000, "message": format!("daemon error: {e}")}
            }),
        };

        writeln!(stdout, "{}", resp).ok();
        stdout.flush().ok();
    }
}

fn forward_jsonrpc(
    sock: &std::path::Path,
    req: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let mut stream = UnixStream::connect(sock).map_err(|e| format!("connect: {e}"))?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .ok();
    writeln!(stream, "{}", req).map_err(|e| format!("write: {e}"))?;
    stream.flush().map_err(|e| format!("flush: {e}"))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("read: {e}"))?;
    serde_json::from_str(line.trim()).map_err(|e| format!("parse: {e}"))
}

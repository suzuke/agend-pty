#![allow(dead_code)]
//! MCP server — stdin NDJSON → API socket → stdout NDJSON.
//! Spawned by CLI agents as their MCP server process.
//! Instance identity via AGEND_INSTANCE_NAME env var.
//!
//! Supports explicit socket path or auto-discovery:
//!   agend-mcp --socket <path>   (explicit API socket)
//!   agend-mcp <agent-name>      (discover via active run dir)

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
        // Fall back to first positional arg (skip --socket pair)
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

        let req: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let id = req.get("id").cloned();
        let method = req["method"].as_str().unwrap_or("");

        let result = match method {
            "initialize" => serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "agend", "version": "0.1.0" }
            }),
            "tools/list" => match api_call(&api_sock, "mcp_tools_list", &serde_json::json!({})) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[mcp] tools/list failed: {e}");
                    serde_json::json!({"tools": []})
                }
            },
            "tools/call" => {
                let tool = req["params"]["name"].as_str().unwrap_or("");
                let args = &req["params"]["arguments"];
                api_call(&api_sock, "mcp_call", &serde_json::json!({
                    "instance": instance, "tool": tool, "arguments": args
                })).unwrap_or_else(|e| serde_json::json!({"content": [{"type": "text", "text": format!("error: {e}")}], "isError": true}))
            }
            "notifications/initialized" | "notifications/cancelled" => continue,
            _ => continue,
        };

        if let Some(id) = id {
            let resp = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
            writeln!(stdout, "{}", resp).ok();
            stdout.flush().ok();
        }
    }
}

fn api_call(
    sock: &std::path::Path,
    method: &str,
    params: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let mut stream = UnixStream::connect(sock).map_err(|e| format!("connect: {e}"))?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .ok();
    let req = serde_json::json!({"method": method, "params": params});
    writeln!(stream, "{}", req).map_err(|e| format!("write: {e}"))?;
    stream.flush().map_err(|e| format!("flush: {e}"))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("read: {e}"))?;
    let resp: serde_json::Value =
        serde_json::from_str(line.trim()).map_err(|e| format!("parse: {e}"))?;
    if resp["ok"].as_bool() == Some(true) {
        Ok(resp["result"].clone())
    } else {
        Err(resp["error"].as_str().unwrap_or("unknown").to_owned())
    }
}

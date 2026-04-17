//! MCP server — identity-injecting bridge to daemon API socket.
//! Spawned by CLI agents as their MCP server process.
//! Instance identity via AGEND_INSTANCE_NAME env var.
//!
//! Forwards all MCP JSON-RPC to daemon's API loopback port with `_instance`
//! field injected. Daemon handles protocol natively.

use agend_pty_poc::{ipc, paths};
use std::io::{BufRead, BufReader, Write};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Parse --port <port> for explicit API port override
    let explicit_port: Option<u16> = if args.len() >= 2 && args[0] == "--port" {
        match args[1].parse() {
            Ok(p) => Some(p),
            Err(_) => {
                eprintln!("[mcp] invalid --port value: {}", args[1]);
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    let instance = std::env::var("AGEND_INSTANCE_NAME").unwrap_or_else(|_| {
        let positional = if explicit_port.is_some() {
            args.get(2)
        } else {
            args.first()
        };
        positional.cloned().unwrap_or_else(|| {
            eprintln!("[mcp] warning: no AGEND_INSTANCE_NAME env or positional arg, running in standalone mode");
            String::new()
        })
    });

    // Resolve API port (retry for daemon startup when not explicit).
    let api_port: u16 = if let Some(p) = explicit_port {
        p
    } else {
        let mut attempts = 0;
        loop {
            if let Some(run) = paths::find_active_run_dir() {
                if let Some(port) = ipc::read_port(&run, ipc::API_NAME) {
                    break port;
                }
            }
            attempts += 1;
            if attempts > 50 {
                eprintln!("[mcp] no daemon API port found after 5s. Start with: agend-pty daemon");
                std::process::exit(1);
            }
            if attempts % 10 == 0 {
                eprintln!("[mcp] waiting for daemon API port...");
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
            Err(e) => {
                let err_resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": {"code": -32700, "message": format!("Parse error: {e}")}
                });
                writeln!(stdout, "{err_resp}").ok();
                stdout.flush().ok();
                continue;
            }
        };

        // Skip notifications (no id → no response expected)
        let id = match req.get("id") {
            Some(id) => id.clone(),
            None => continue,
        };

        // Inject instance identity so daemon knows who's calling
        req["_instance"] = serde_json::json!(instance);

        // Forward to daemon API port (which handles MCP JSON-RPC natively)
        let resp = match forward_jsonrpc(api_port, &req) {
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

fn forward_jsonrpc(port: u16, req: &serde_json::Value) -> Result<serde_json::Value, String> {
    let mut stream = ipc::connect_port(port).map_err(|e| format!("connect: {e}"))?;
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

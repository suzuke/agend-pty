#![allow(dead_code, unused_imports, clippy::unwrap_used)]
//! MCP server — stdin NDJSON → API socket → stdout NDJSON.
//! Spawned by CLI agents as their MCP server process.
//! Instance identity via AGEND_INSTANCE_NAME env var.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

#[path = "paths.rs"]
mod paths;

fn main() {
    let instance = std::env::var("AGEND_INSTANCE_NAME").unwrap_or_else(|_| {
        std::env::args().nth(1).unwrap_or_else(|| {
            eprintln!("AGEND_INSTANCE_NAME not set");
            std::process::exit(1);
        })
    });

    // Find API socket (retry for daemon startup)
    let api_sock = {
        let mut attempts = 0;
        loop {
            if let Some(run) = paths::find_active_run_dir() {
                let sock = run.join("api.sock");
                if sock.exists() { break sock; }
            }
            attempts += 1;
            if attempts > 50 {
                eprintln!("[mcp] no daemon API socket found after 5s");
                std::process::exit(1);
            }
            if attempts % 10 == 0 { eprintln!("[mcp] waiting for daemon API socket..."); }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    };

    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut stdout = std::io::stdout();
    let mut line = String::new();

    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 { break; }
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }

        let req: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v, Err(_) => continue,
        };

        let id = req.get("id").cloned();
        let method = req["method"].as_str().unwrap_or("");

        let result = match method {
            "initialize" => serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "agend", "version": "0.1.0" }
            }),
            "tools/list" => {
                api_call(&api_sock, "mcp_tools_list", &serde_json::json!({}))
                    .unwrap_or_else(|_| tools_list_fallback())
            }
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

fn api_call(sock: &std::path::Path, method: &str, params: &serde_json::Value) -> Result<serde_json::Value, String> {
    let mut stream = UnixStream::connect(sock).map_err(|e| format!("connect: {e}"))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30))).ok();
    let req = serde_json::json!({"method": method, "params": params});
    writeln!(stream, "{}", req).map_err(|e| format!("write: {e}"))?;
    stream.flush().map_err(|e| format!("flush: {e}"))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| format!("read: {e}"))?;
    let resp: serde_json::Value = serde_json::from_str(line.trim()).map_err(|e| format!("parse: {e}"))?;
    if resp["ok"].as_bool() == Some(true) {
        Ok(resp["result"].clone())
    } else {
        Err(resp["error"].as_str().unwrap_or("unknown").to_owned())
    }
}

fn tools_list_fallback() -> serde_json::Value {
    serde_json::json!({"tools": [
        {"name":"reply","description":"Reply to Telegram user.","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}},
        {"name":"send_to_instance","description":"Send message to another agent.","inputSchema":{"type":"object","properties":{"instance_name":{"type":"string"},"message":{"type":"string"}},"required":["instance_name","message"]}},
        {"name":"broadcast","description":"Send to all agents.","inputSchema":{"type":"object","properties":{"message":{"type":"string"}},"required":["message"]}},
        {"name":"list_instances","description":"List running agents.","inputSchema":{"type":"object","properties":{}}},
        {"name":"inbox","description":"Read inbox messages.","inputSchema":{"type":"object","properties":{"id":{"type":"integer"}}}}
    ]})
}

//! API socket — JSON request/response for fleet management.
//!
//! Listens on ~/.agend/run/<pid>/api.sock
//! Protocol: newline-delimited JSON (one request per line, one response per line)

use crate::paths;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};

#[derive(Debug, Deserialize)]
pub struct ApiRequest {
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize)]
pub struct ApiResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub type PtyWriter = Arc<Mutex<Box<dyn Write + Send>>>;
pub type AgentWriters = Arc<Mutex<HashMap<String, PtyWriter>>>;

/// Start the API socket server in a new thread.
pub fn start(writers: AgentWriters) {
    let sock = paths::run_dir().join("api.sock");
    let _ = std::fs::remove_file(&sock);
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => { eprintln!("[api] bind error: {e}"); return; }
    };
    eprintln!("[api] listening on {}", sock.display());

    std::thread::Builder::new()
        .name("api_server".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let w = Arc::clone(&writers);
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                    let mut writer = stream;
                    let mut line = String::new();
                    while reader.read_line(&mut line).unwrap_or(0) > 0 {
                        let resp = match serde_json::from_str::<ApiRequest>(line.trim()) {
                            Ok(req) => handle_request(&req, &w),
                            Err(e) => ApiResponse { ok: false, result: None, error: Some(format!("parse: {e}")) },
                        };
                        let _ = writeln!(writer, "{}", serde_json::to_string(&resp).unwrap_or_default());
                        let _ = writer.flush();
                        line.clear();
                    }
                });
            }
        })
        .unwrap();
}

fn handle_request(req: &ApiRequest, writers: &AgentWriters) -> ApiResponse {
    match req.method.as_str() {
        "list" => {
            let names: Vec<String> = writers.lock().unwrap_or_else(|e| e.into_inner())
                .keys().cloned().collect();
            ApiResponse { ok: true, result: Some(json!({"instances": names})), error: None }
        }
        "inject" => {
            let target = req.params["instance"].as_str().unwrap_or("");
            let message = req.params["message"].as_str().unwrap_or("");
            let sender = req.params["sender"].as_str().unwrap_or("api");
            if target.is_empty() || message.is_empty() {
                return ApiResponse { ok: false, result: None, error: Some("instance and message required".into()) };
            }
            let w = writers.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(pw) = w.get(target) {
                let formatted = format!("[message from {sender} (reply via send_to_instance to \"{sender}\")] {message}\r");
                match pw.lock().unwrap_or_else(|e| e.into_inner()).write_all(formatted.as_bytes()) {
                    Ok(_) => {
                        eprintln!("[api] {sender} → {target}: {}", message.chars().take(80).collect::<String>());
                        ApiResponse { ok: true, result: Some(json!({"sent": true})), error: None }
                    }
                    Err(e) => ApiResponse { ok: false, result: None, error: Some(format!("write: {e}")) }
                }
            } else {
                ApiResponse { ok: false, result: None, error: Some(format!("instance '{target}' not found")) }
            }
        }
        "kill" => {
            let target = req.params["instance"].as_str().unwrap_or("");
            if target.is_empty() {
                return ApiResponse { ok: false, result: None, error: Some("instance required".into()) };
            }
            // Send Ctrl+C then Ctrl+D to the PTY
            let w = writers.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(pw) = w.get(target) {
                let _ = pw.lock().unwrap_or_else(|e| e.into_inner()).write_all(b"\x03\x04");
                eprintln!("[api] killed {target}");
                ApiResponse { ok: true, result: Some(json!({"killed": target})), error: None }
            } else {
                ApiResponse { ok: false, result: None, error: Some(format!("instance '{target}' not found")) }
            }
        }
        "status" => {
            let names: Vec<Value> = writers.lock().unwrap_or_else(|e| e.into_inner())
                .keys().map(|n| json!({"name": n, "status": "running"})).collect();
            ApiResponse { ok: true, result: Some(json!({"agents": names})), error: None }
        }
        _ => ApiResponse { ok: false, result: None, error: Some(format!("unknown method: {}", req.method)) }
    }
}

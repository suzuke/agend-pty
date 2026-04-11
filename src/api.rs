//! API socket — JSON request/response for fleet management + MCP tool dispatch.
//!
//! Listens on ~/.agend/run/<pid>/api.sock
//! Protocol: newline-delimited JSON (one request per line, one response per line)

use crate::{channel, inbox, paths};
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

/// Shared daemon context for API handlers.
pub struct DaemonCtx {
    pub writers: AgentWriters,
    pub inbox: Arc<inbox::InboxStore>,
    pub channel_mgr: Arc<Mutex<channel::ChannelManager>>,
}

/// Start the API socket server in a new thread.
pub fn start(ctx: Arc<DaemonCtx>) {
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
                let c = Arc::clone(&ctx);
                std::thread::spawn(move || {
                    let cloned = match stream.try_clone() {
                        Ok(s) => s,
                        Err(e) => { eprintln!("[api] stream clone failed: {e}"); return; }
                    };
                    let mut reader = BufReader::new(cloned);
                    let mut writer = stream;
                    let mut line = String::new();
                    while reader.read_line(&mut line).unwrap_or(0) > 0 {
                        let resp = match serde_json::from_str::<ApiRequest>(line.trim()) {
                            Ok(req) => handle_request(&req, &c),
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

fn ok(result: Value) -> ApiResponse { ApiResponse { ok: true, result: Some(result), error: None } }
fn err(msg: impl Into<String>) -> ApiResponse { ApiResponse { ok: false, result: None, error: Some(msg.into()) } }

fn handle_request(req: &ApiRequest, ctx: &DaemonCtx) -> ApiResponse {
    match req.method.as_str() {
        // ── Fleet management ──
        "list" => {
            let names: Vec<String> = ctx.writers.lock().unwrap_or_else(|e| e.into_inner())
                .keys().cloned().collect();
            ok(json!({"instances": names}))
        }
        "status" => {
            let names: Vec<Value> = ctx.writers.lock().unwrap_or_else(|e| e.into_inner())
                .keys().map(|n| json!({"name": n, "status": "running"})).collect();
            ok(json!({"agents": names}))
        }
        "inject" => {
            let target = req.params["instance"].as_str().unwrap_or("");
            let message = req.params["message"].as_str().unwrap_or("");
            let sender = req.params["sender"].as_str().unwrap_or("api");
            if target.is_empty() || message.is_empty() {
                return err("instance and message required");
            }
            inject_message(ctx, sender, target, message)
        }
        "kill" => {
            let target = req.params["instance"].as_str().unwrap_or("");
            if target.is_empty() { return err("instance required"); }
            let w = ctx.writers.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(pw) = w.get(target) {
                let _ = pw.lock().unwrap_or_else(|e| e.into_inner()).write_all(b"\x03\x04");
                ok(json!({"killed": target}))
            } else { err(format!("instance '{target}' not found")) }
        }

        // ── MCP tool dispatch (called by agend-pty mcp) ──
        "mcp_call" => {
            let instance = req.params["instance"].as_str().unwrap_or("");
            let tool = req.params["tool"].as_str().unwrap_or("");
            let args = &req.params["arguments"];
            let result = handle_mcp_tool(ctx, instance, tool, args);
            ok(result)
        }
        "mcp_tools_list" => {
            ok(mcp_tools_list())
        }

        _ => err(format!("unknown method: {}", req.method))
    }
}

fn handle_mcp_tool(ctx: &DaemonCtx, instance: &str, tool: &str, args: &Value) -> Value {
    match tool {
        "send_to_instance" => {
            let target = args["instance_name"].as_str().unwrap_or("");
            let message = args["message"].as_str().unwrap_or("");
            match inject_message(ctx, instance, target, message) {
                ApiResponse { ok: true, .. } => json!({"content": [{"type": "text", "text": format!("{{\"sent\":true,\"target\":\"{target}\"}}")}]}),
                ApiResponse { error: Some(e), .. } => json!({"content": [{"type": "text", "text": e}], "isError": true}),
                _ => json!({"content": [{"type": "text", "text": "unknown error"}], "isError": true}),
            }
        }
        "broadcast" => {
            let message = args["message"].as_str().unwrap_or("");
            let names: Vec<String> = ctx.writers.lock().unwrap_or_else(|e| e.into_inner())
                .keys().filter(|k| *k != instance).cloned().collect();
            for target in &names { inject_message(ctx, instance, target, message); }
            json!({"content": [{"type": "text", "text": format!("{{\"broadcast\":true,\"sent_to\":{}}}", json!(names))}]})
        }
        "list_instances" => {
            let names: Vec<String> = ctx.writers.lock().unwrap_or_else(|e| e.into_inner()).keys().cloned().collect();
            json!({"content": [{"type": "text", "text": json!({"instances": names}).to_string()}]})
        }
        "describe_instance" => {
            let name = args["name"].as_str().unwrap_or("");
            let w = ctx.writers.lock().unwrap_or_else(|e| e.into_inner());
            if w.contains_key(name) {
                json!({"content": [{"type": "text", "text": json!({"name": name, "status": "running"}).to_string()}]})
            } else {
                json!({"content": [{"type": "text", "text": format!("instance '{name}' not found")}], "isError": true})
            }
        }
        "request_information" => {
            let target = args["target_instance"].as_str().unwrap_or("");
            let question = args["question"].as_str().unwrap_or("");
            let ctx_text = args["context"].as_str().unwrap_or("");
            let msg = if ctx_text.is_empty() { format!("[query from {instance}] {question}") }
            else { format!("[query from {instance}] {question}\n\nContext: {ctx_text}") };
            inject_message(ctx, instance, target, &msg);
            json!({"content": [{"type": "text", "text": format!("{{\"sent\":true,\"target\":\"{target}\"}}")}]})
        }
        "delegate_task" => {
            let target = args["target_instance"].as_str().unwrap_or("");
            let task = args["task"].as_str().unwrap_or("");
            let criteria = args["success_criteria"].as_str().unwrap_or("");
            let ctx_text = args["context"].as_str().unwrap_or("");
            let mut msg = format!("[task from {instance}] {task}");
            if !criteria.is_empty() { msg.push_str(&format!("\n\nSuccess criteria: {criteria}")); }
            if !ctx_text.is_empty() { msg.push_str(&format!("\n\nContext: {ctx_text}")); }
            inject_message(ctx, instance, target, &msg);
            json!({"content": [{"type": "text", "text": format!("{{\"sent\":true,\"target\":\"{target}\"}}")}]})
        }
        "report_result" => {
            let target = args["target_instance"].as_str().unwrap_or("");
            let summary = args["summary"].as_str().unwrap_or("");
            let artifacts = args["artifacts"].as_str().unwrap_or("");
            let mut msg = format!("[result from {instance}] {summary}");
            if !artifacts.is_empty() { msg.push_str(&format!("\n\nArtifacts: {artifacts}")); }
            inject_message(ctx, instance, target, &msg);
            json!({"content": [{"type": "text", "text": format!("{{\"sent\":true,\"target\":\"{target}\"}}")}]})
        }
        "reply" => {
            let text = args["text"].as_str().unwrap_or("");
            let formatted = format!("[{instance}] {text}");
            ctx.channel_mgr.lock().unwrap_or_else(|e| e.into_inner()).send_to_agent(instance, &formatted);
            json!({"content": [{"type": "text", "text": "{\"replied\":true}"}]})
        }
        "inbox" => {
            if let Some(id) = args["id"].as_u64() {
                match ctx.inbox.get(instance, id) {
                    Some(msg) => json!({"content": [{"type": "text", "text": format!("[from {}] {}", msg.sender, msg.text)}]}),
                    None => json!({"content": [{"type": "text", "text": "message not found"}], "isError": true}),
                }
            } else {
                let msgs = ctx.inbox.list(instance);
                let list: Vec<Value> = msgs.iter().map(|m| json!({"id": m.id, "sender": m.sender, "preview": m.text.chars().take(100).collect::<String>()})).collect();
                json!({"content": [{"type": "text", "text": json!({"messages": list}).to_string()}]})
            }
        }
        "delete_instance" => {
            let name = args["name"].as_str().unwrap_or("");
            let w = ctx.writers.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(pw) = w.get(name) {
                let _ = pw.lock().unwrap_or_else(|e| e.into_inner()).write_all(b"\x03\x04");
                json!({"content": [{"type": "text", "text": format!("{{\"deleted\":\"{name}\"}}")}]})
            } else {
                json!({"content": [{"type": "text", "text": format!("instance '{name}' not found")}], "isError": true})
            }
        }
        _ => json!({"content": [{"type": "text", "text": format!("unknown tool: {tool}")}], "isError": true}),
    }
}

fn inject_message(ctx: &DaemonCtx, sender: &str, target: &str, message: &str) -> ApiResponse {
    let w = ctx.writers.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(pw) = w.get(target) {
        // Use inbox for smart injection
        let text = match ctx.inbox.store_or_inject(target, sender, message, "\r") {
            crate::inbox::InjectAction::Direct(t) | crate::inbox::InjectAction::Notification(t) => t,
        };
        match pw.lock().unwrap_or_else(|e| e.into_inner()).write_all(text.as_bytes()) {
            Ok(_) => {
                eprintln!("[api] {sender} → {target}: {}", message.chars().take(80).collect::<String>());
                ok(json!({"sent": true}))
            }
            Err(e) => err(format!("write: {e}"))
        }
    } else {
        err(format!("instance '{target}' not found"))
    }
}

pub fn mcp_tools_list() -> Value {
    json!({"tools": [
        {"name":"reply","description":"Reply to a Telegram user.","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}},
        {"name":"send_to_instance","description":"Send a message to another agent instance.","inputSchema":{"type":"object","properties":{"instance_name":{"type":"string"},"message":{"type":"string"},"request_kind":{"type":"string","enum":["query","task","report","update"]},"requires_reply":{"type":"boolean"},"correlation_id":{"type":"string"}},"required":["instance_name","message"]}},
        {"name":"request_information","description":"Ask another agent a question.","inputSchema":{"type":"object","properties":{"target_instance":{"type":"string"},"question":{"type":"string"},"context":{"type":"string"}},"required":["target_instance","question"]}},
        {"name":"delegate_task","description":"Delegate a task to another agent.","inputSchema":{"type":"object","properties":{"target_instance":{"type":"string"},"task":{"type":"string"},"success_criteria":{"type":"string"},"context":{"type":"string"}},"required":["target_instance","task"]}},
        {"name":"report_result","description":"Report results back.","inputSchema":{"type":"object","properties":{"target_instance":{"type":"string"},"summary":{"type":"string"},"correlation_id":{"type":"string"},"artifacts":{"type":"string"}},"required":["target_instance","summary"]}},
        {"name":"broadcast","description":"Send to all agents.","inputSchema":{"type":"object","properties":{"message":{"type":"string"}},"required":["message"]}},
        {"name":"list_instances","description":"List running agents.","inputSchema":{"type":"object","properties":{}}},
        {"name":"describe_instance","description":"Get agent details.","inputSchema":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}},
        {"name":"delete_instance","description":"Stop an agent.","inputSchema":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}},
        {"name":"inbox","description":"Read inbox messages.","inputSchema":{"type":"object","properties":{"id":{"type":"integer"}}}}
    ]})
}

//! API socket — JSON request/response for fleet management + MCP tool dispatch.
//!
//! Listens on ~/.agend/run/<pid>/api.sock
//! Protocol: newline-delimited JSON (one request per line, one response per line)

use crate::{channel, event_log, fleet_store, git, health, inbox, paths, scheduler, state};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// Agent state handle exposed to API layer.
pub struct AgentStateHandle {
    pub state_machine: Arc<Mutex<state::StateMachine>>,
    pub health: Arc<Mutex<health::HealthMonitor>>,
    pub working_dir: Option<std::path::PathBuf>,
}
pub type AgentStateMap = Arc<Mutex<HashMap<String, AgentStateHandle>>>;

/// Shared daemon context — holds all shared state.
/// Minimal spawn info for create_instance.
#[derive(Clone)]
pub struct SpawnConfigInfo {
    pub command: String,
    pub working_dir: Option<std::path::PathBuf>,
    pub worktree: bool,
    pub branch: Option<String>,
}

pub struct DaemonCtx {
    pub writers: AgentWriters,
    pub states: AgentStateMap,
    pub spawn_configs: Arc<Mutex<HashMap<String, SpawnConfigInfo>>>,
    pub inbox: Arc<inbox::InboxStore>,
    pub channel_mgr: Arc<Mutex<channel::ChannelManager>>,
}

/// Start the API socket server in a new thread.
pub fn start(ctx: Arc<DaemonCtx>) {
    let sock = paths::run_dir().join("api.sock");
    let _ = std::fs::remove_file(&sock);
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, "API bind error");
            return;
        }
    };
    tracing::info!(path = %sock.display(), "API listening");

    std::thread::Builder::new()
        .name("api_server".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let c = Arc::clone(&ctx);
                std::thread::spawn(move || {
                    let cloned = match stream.try_clone() {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::error!(error = %e, "API stream clone failed");
                            return;
                        }
                    };
                    let mut reader = BufReader::new(cloned);
                    let mut writer = stream;
                    let mut line = String::new();
                    while reader.read_line(&mut line).unwrap_or(0) > 0 {
                        let resp = match serde_json::from_str::<ApiRequest>(line.trim()) {
                            Ok(req) => handle_request(&req, &c),
                            Err(e) => ApiResponse {
                                ok: false,
                                result: None,
                                error: Some(format!("parse: {e}")),
                            },
                        };
                        let _ = writeln!(
                            writer,
                            "{}",
                            serde_json::to_string(&resp).unwrap_or_default()
                        );
                        let _ = writer.flush();
                        line.clear();
                    }
                });
            }
        })
        .ok(); // thread spawn is infallible
}

fn ok(result: Value) -> ApiResponse {
    ApiResponse {
        ok: true,
        result: Some(result),
        error: None,
    }
}
fn err(msg: impl Into<String>) -> ApiResponse {
    ApiResponse {
        ok: false,
        result: None,
        error: Some(msg.into()),
    }
}

fn handle_request(req: &ApiRequest, ctx: &DaemonCtx) -> ApiResponse {
    match req.method.as_str() {
        // ── Fleet management ──
        "list" => {
            let names: Vec<String> = ctx
                .writers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .keys()
                .cloned()
                .collect();
            ok(json!({"instances": names}))
        }
        "status" => {
            let names: Vec<Value> = ctx
                .writers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .keys()
                .map(|n| json!({"name": n, "status": "running"}))
                .collect();
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
            if target.is_empty() {
                return err("instance required");
            }
            let w = ctx.writers.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(pw) = w.get(target) {
                let _ = pw
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .write_all(b"\x03\x04");
                ok(json!({"killed": target}))
            } else {
                err(format!("instance '{target}' not found"))
            }
        }

        // ── MCP tool dispatch (called by agend-pty mcp) ──
        "mcp_call" => {
            let instance = req.params["instance"].as_str().unwrap_or("");
            let tool = req.params["tool"].as_str().unwrap_or("");
            let args = &req.params["arguments"];
            let result = handle_mcp_tool(ctx, instance, tool, args);
            ok(result)
        }
        "mcp_tools_list" => ok(mcp_tools_list()),

        _ => err(format!("unknown method: {}", req.method)),
    }
}

fn handle_mcp_tool(ctx: &DaemonCtx, instance: &str, tool: &str, args: &Value) -> Value {
    match tool {
        "send_to_instance" => {
            let target = args["instance_name"].as_str().unwrap_or("");
            let message = args["message"].as_str().unwrap_or("");
            match inject_message(ctx, instance, target, message) {
                ApiResponse { ok: true, .. } => {
                    json!({"content": [{"type": "text", "text": format!("{{\"sent\":true,\"target\":\"{target}\"}}")}]})
                }
                ApiResponse { error: Some(e), .. } => {
                    json!({"content": [{"type": "text", "text": e}], "isError": true})
                }
                _ => {
                    json!({"content": [{"type": "text", "text": "unknown error"}], "isError": true})
                }
            }
        }
        "broadcast" => {
            let message = args["message"].as_str().unwrap_or("");
            let team = args["team"].as_str();
            let team_members = team.and_then(fleet_store::get_team_members);
            let names: Vec<String> = ctx
                .writers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .keys()
                .filter(|k| *k != instance)
                .filter(|k| team_members.as_ref().map(|m| m.contains(k)).unwrap_or(true))
                .cloned()
                .collect();
            for target in &names {
                inject_message(ctx, instance, target, message);
            }
            let skipped: Vec<String> = team_members
                .as_ref()
                .map(|m| {
                    m.iter()
                        .filter(|k| !names.contains(k) && k.as_str() != instance)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            json!({"content": [{"type": "text", "text": json!({"broadcast": true, "sent_to": names, "skipped": skipped}).to_string()}]})
        }
        "list_instances" => {
            let names: Vec<String> = ctx
                .writers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .keys()
                .cloned()
                .collect();
            json!({"content": [{"type": "text", "text": json!({"instances": names}).to_string()}]})
        }
        "describe_instance" => {
            let name = args["instance_name"]
                .as_str()
                .or_else(|| args["name"].as_str())
                .unwrap_or("");
            let w = ctx.writers.lock().unwrap_or_else(|e| e.into_inner());
            if w.contains_key(name) {
                json!({"content": [{"type": "text", "text": json!({"name": name, "status": "running"}).to_string()}]})
            } else {
                json!({"content": [{"type": "text", "text": format!("instance '{name}' not found")}], "isError": true})
            }
        }
        "request_information" => {
            let target = args["instance_name"]
                .as_str()
                .or_else(|| args["target_instance"].as_str())
                .unwrap_or("");
            let question = args["question"].as_str().unwrap_or("");
            let ctx_text = args["context"].as_str().unwrap_or("");
            let msg = if ctx_text.is_empty() {
                format!("[query from {instance}] {question}")
            } else {
                format!("[query from {instance}] {question}\n\nContext: {ctx_text}")
            };
            inject_message(ctx, instance, target, &msg);
            json!({"content": [{"type": "text", "text": format!("{{\"sent\":true,\"target\":\"{target}\"}}")}]})
        }
        "delegate_task" => {
            let target = args["instance_name"]
                .as_str()
                .or_else(|| args["target_instance"].as_str())
                .unwrap_or("");
            let task = args["task"].as_str().unwrap_or("");
            let criteria = args["success_criteria"].as_str().unwrap_or("");
            let ctx_text = args["context"].as_str().unwrap_or("");
            let mut msg = format!("[task from {instance}] {task}");
            if !criteria.is_empty() {
                msg.push_str(&format!("\n\nSuccess criteria: {criteria}"));
            }
            if !ctx_text.is_empty() {
                msg.push_str(&format!("\n\nContext: {ctx_text}"));
            }
            inject_message(ctx, instance, target, &msg);
            json!({"content": [{"type": "text", "text": format!("{{\"sent\":true,\"target\":\"{target}\"}}")}]})
        }
        "report_result" => {
            let target = args["instance_name"]
                .as_str()
                .or_else(|| args["target_instance"].as_str())
                .unwrap_or("");
            let summary = args["summary"].as_str().unwrap_or("");
            let artifacts = args["artifacts"].as_str().unwrap_or("");
            let mut msg = format!("[result from {instance}] {summary}");
            if !artifacts.is_empty() {
                msg.push_str(&format!("\n\nArtifacts: {artifacts}"));
            }
            inject_message(ctx, instance, target, &msg);
            json!({"content": [{"type": "text", "text": format!("{{\"sent\":true,\"target\":\"{target}\"}}")}]})
        }
        "reply" => {
            let text = args["text"].as_str().unwrap_or("");
            let format_mode = args["format"].as_str().unwrap_or("text");
            let reply_to = args["reply_to"].as_str();
            let formatted = format!("[{instance}] {text}");
            let msg_id = ctx
                .channel_mgr
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .send_to_agent_ext(instance, &formatted, format_mode, reply_to);
            let id_str = msg_id.unwrap_or_default();
            json!({"content": [{"type": "text", "text": json!({"replied": true, "message_id": id_str}).to_string()}]})
        }
        "inbox" => {
            if let Some(id) = args["id"].as_u64() {
                match ctx.inbox.get(instance, id) {
                    Some(msg) => {
                        json!({"content": [{"type": "text", "text": format!("[from {}] {}", msg.sender, msg.text)}]})
                    }
                    None => {
                        json!({"content": [{"type": "text", "text": "message not found"}], "isError": true})
                    }
                }
            } else {
                let msgs = ctx.inbox.list(instance);
                let list: Vec<Value> = msgs.iter().map(|m| json!({"id": m.id, "sender": m.sender, "preview": m.text.chars().take(100).collect::<String>()})).collect();
                json!({"content": [{"type": "text", "text": json!({"messages": list}).to_string()}]})
            }
        }
        "delete_instance" => {
            let name = args["instance_name"]
                .as_str()
                .or_else(|| args["name"].as_str())
                .unwrap_or("");
            let cleanup_wt = args["cleanup_worktree"].as_bool().unwrap_or(false);
            let w = ctx.writers.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(pw) = w.get(name) {
                let _ = pw
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .write_all(b"\x03\x04");
                drop(w);
                let mut resp = json!({"deleted": name});
                if cleanup_wt {
                    // Check for uncommitted changes + remove worktree
                    let wd = ctx
                        .states
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(name)
                        .and_then(|h| h.working_dir.clone());
                    if let Some(wd) = wd {
                        let dirty = std::process::Command::new("git")
                            .args(["-C", &wd.display().to_string(), "status", "--porcelain"])
                            .output()
                            .ok()
                            .map(|o| !o.stdout.is_empty())
                            .unwrap_or(false);
                        if dirty {
                            resp["warning"] = json!("uncommitted changes were discarded");
                        }
                        if let Err(e) = git::remove_worktree(&wd, name) {
                            resp["worktree_error"] = json!(e);
                        } else {
                            resp["worktree_removed"] = json!(true);
                        }
                    }
                }
                json!({"content": [{"type": "text", "text": resp.to_string()}]})
            } else {
                json!({"content": [{"type": "text", "text": format!("instance '{name}' not found")}], "isError": true})
            }
        }
        "post_decision" => {
            let title = args["title"].as_str().unwrap_or("");
            let content = args["content"].as_str().unwrap_or("");
            let d = fleet_store::post_decision(instance, title, content);
            json!({"content": [{"type": "text", "text": json!({"posted": true, "id": d.id}).to_string()}]})
        }
        "list_decisions" => {
            let decisions = fleet_store::list_decisions();
            let list: Vec<Value> = decisions
                .iter()
                .map(|d| json!({"id": d.id, "title": d.title, "author": d.author}))
                .collect();
            json!({"content": [{"type": "text", "text": json!({"decisions": list}).to_string()}]})
        }
        "task" => {
            let action = args["action"].as_str().unwrap_or("");
            match action {
                "create" => {
                    let title = args["title"].as_str().unwrap_or("untitled");
                    let desc = args["description"].as_str().unwrap_or("");
                    let assignee = args["assignee"].as_str().unwrap_or("");
                    let t = fleet_store::create_task(instance, title, desc, assignee);
                    json!({"content": [{"type": "text", "text": json!({"created": t.id}).to_string()}]})
                }
                "list" => {
                    let tasks = fleet_store::list_tasks();
                    let list: Vec<Value> = tasks.iter().map(|t| json!({"id": t.id, "title": t.title, "status": t.status, "assignee": t.assignee})).collect();
                    json!({"content": [{"type": "text", "text": json!({"tasks": list}).to_string()}]})
                }
                "claim" => {
                    let id = args["id"].as_str().unwrap_or("");
                    match fleet_store::update_task(id, Some("claimed"), Some(instance), None) {
                        Some(t) => {
                            json!({"content": [{"type": "text", "text": json!({"claimed": t.id}).to_string()}]})
                        }
                        None => {
                            json!({"content": [{"type": "text", "text": "task not found"}], "isError": true})
                        }
                    }
                }
                "done" => {
                    let id = args["id"].as_str().unwrap_or("");
                    let result = args["result"].as_str().unwrap_or("");
                    match fleet_store::update_task(id, Some("done"), None, Some(result)) {
                        Some(t) => {
                            json!({"content": [{"type": "text", "text": json!({"done": t.id}).to_string()}]})
                        }
                        None => {
                            json!({"content": [{"type": "text", "text": "task not found"}], "isError": true})
                        }
                    }
                }
                "update" => {
                    let id = args["id"].as_str().unwrap_or("");
                    let status = args["status"].as_str();
                    let assignee = args["assignee"].as_str();
                    match fleet_store::update_task(id, status, assignee, None) {
                        Some(t) => {
                            json!({"content": [{"type": "text", "text": json!({"updated": t.id}).to_string()}]})
                        }
                        None => {
                            json!({"content": [{"type": "text", "text": "task not found"}], "isError": true})
                        }
                    }
                }
                _ => {
                    json!({"content": [{"type": "text", "text": format!("unknown task action: {action}")}], "isError": true})
                }
            }
        }
        "react" => {
            let message_id = args["message_id"].as_str().unwrap_or("");
            let emoji = args["emoji"].as_str().unwrap_or("");
            match ctx
                .channel_mgr
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .react(instance, message_id, emoji)
            {
                Ok(()) => json!({"content": [{"type": "text", "text": "{\"reacted\":true}"}]}),
                Err(e) => json!({"content": [{"type": "text", "text": e}], "isError": true}),
            }
        }
        "edit_message" => {
            let message_id = args["message_id"].as_str().unwrap_or("");
            let text = args["text"].as_str().unwrap_or("");
            match ctx
                .channel_mgr
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .edit_message(instance, message_id, text)
            {
                Ok(()) => json!({"content": [{"type": "text", "text": "{\"edited\":true}"}]}),
                Err(e) => json!({"content": [{"type": "text", "text": e}], "isError": true}),
            }
        }
        "wait_for_idle" => {
            let target = args["instance_name"].as_str().unwrap_or("");
            let timeout = args["timeout_secs"].as_u64().unwrap_or(120).min(300);
            let deadline = Instant::now() + Duration::from_secs(timeout);
            loop {
                let agent_state = ctx
                    .states
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(target)
                    .and_then(|h| h.state_machine.lock().ok().map(|s| s.state()));
                match agent_state {
                    Some(s @ (state::AgentState::Ready | state::AgentState::Idle)) => {
                        break json!({"content": [{"type": "text", "text": json!({"idle": true, "state": format!("{s:?}")}).to_string()}]})
                    }
                    Some(s @ (state::AgentState::Crashed | state::AgentState::Errored)) => {
                        break json!({"content": [{"type": "text", "text": format!("agent '{target}' is {s:?}")}], "isError": true})
                    }
                    None => {
                        break json!({"content": [{"type": "text", "text": format!("instance '{target}' not found")}], "isError": true})
                    }
                    _ => {}
                }
                if Instant::now() > deadline {
                    break json!({"content": [{"type": "text", "text": format!("timeout after {timeout}s waiting for '{target}'")}], "isError": true});
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        }
        "merge_preview" => {
            let target = args["instance_name"].as_str().unwrap_or(instance);
            let branch = format!("agend/{target}");
            let repo = ctx
                .states
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(target)
                .and_then(|h| h.working_dir.clone());
            let repo = match repo {
                Some(p) => p,
                None => {
                    return json!({"content": [{"type": "text", "text": format!("instance '{target}' not found")}], "isError": true})
                }
            };
            match git::merge_preview(&repo, &branch) {
                Ok(p) => json!({"content": [{"type": "text", "text": json!({
                    "diff_stat": p.diff_stat, "files_changed": p.files_changed, "has_conflicts": p.has_conflicts
                }).to_string()}]}),
                Err(e) => json!({"content": [{"type": "text", "text": e}], "isError": true}),
            }
        }
        "merge_agent" => {
            let target = args["instance_name"].as_str().unwrap_or(instance);
            let default_msg = format!("merge agent/{target}");
            let message = args["message"].as_str().unwrap_or(&default_msg);
            let branch = format!("agend/{target}");
            let repo = ctx
                .states
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(target)
                .and_then(|h| h.working_dir.clone());
            let repo = match repo {
                Some(p) => p,
                None => {
                    return json!({"content": [{"type": "text", "text": format!("instance '{target}' not found")}], "isError": true})
                }
            };
            match git::squash_merge(&repo, &branch, message) {
                Ok(()) => json!({"content": [{"type": "text", "text": "{\"merged\":true}"}]}),
                Err(e) => json!({"content": [{"type": "text", "text": e}], "isError": true}),
            }
        }
        "update_decision" => {
            let id = args["id"].as_u64().unwrap_or(0);
            let title = args["title"].as_str();
            let content = args["content"].as_str();
            match fleet_store::update_decision(id, title, content) {
                Some(d) => {
                    json!({"content": [{"type": "text", "text": json!({"updated": d.id}).to_string()}]})
                }
                None => {
                    json!({"content": [{"type": "text", "text": "decision not found"}], "isError": true})
                }
            }
        }
        "team" => {
            let action = args["action"].as_str().unwrap_or("");
            match action {
                "create" => {
                    let name = args["name"].as_str().unwrap_or("");
                    let members: Vec<String> = args["members"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    let t = fleet_store::create_team(name, &members);
                    json!({"content": [{"type": "text", "text": json!({"created": t.name}).to_string()}]})
                }
                "list" => {
                    let teams = fleet_store::list_teams();
                    let list: Vec<Value> = teams
                        .iter()
                        .map(|t| json!({"name": t.name, "members": t.members}))
                        .collect();
                    json!({"content": [{"type": "text", "text": json!({"teams": list}).to_string()}]})
                }
                "delete" => {
                    let name = args["name"].as_str().unwrap_or("");
                    fleet_store::delete_team(name);
                    json!({"content": [{"type": "text", "text": json!({"deleted": name}).to_string()}]})
                }
                "update" => {
                    let name = args["name"].as_str().unwrap_or("");
                    let members: Vec<String> = args["members"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    match fleet_store::update_team(name, &members) {
                        Some(t) => {
                            json!({"content": [{"type": "text", "text": json!({"updated": t.name}).to_string()}]})
                        }
                        None => {
                            json!({"content": [{"type": "text", "text": "team not found"}], "isError": true})
                        }
                    }
                }
                _ => {
                    json!({"content": [{"type": "text", "text": format!("unknown team action: {action}")}], "isError": true})
                }
            }
        }
        "list_events" => {
            let agent = args["agent"].as_str();
            let etype = args["type"].as_str();
            let events = event_log::list_events(agent, etype);
            let list: Vec<Value> = events.iter().map(|e| json!({"ts": e.ts, "type": e.event_type, "agent": e.agent, "details": e.details})).collect();
            json!({"content": [{"type": "text", "text": json!({"events": list}).to_string()}]})
        }
        "schedule" => {
            let action = args["action"].as_str().unwrap_or("");
            match action {
                "create" => {
                    let cron = args["cron"].as_str().unwrap_or("* * * * *");
                    let target = args["target"].as_str().unwrap_or("");
                    let message = args["message"].as_str().unwrap_or("");
                    match scheduler::create_schedule(cron, target, message) {
                        Ok(s) => {
                            json!({"content": [{"type": "text", "text": json!({"created": s.id}).to_string()}]})
                        }
                        Err(e) => {
                            json!({"content": [{"type": "text", "text": e}], "isError": true})
                        }
                    }
                }
                "list" => {
                    let schedules = scheduler::list_schedules();
                    let list: Vec<Value> = schedules.iter().map(|s| json!({"id": s.id, "cron": s.cron, "target": s.target, "message": s.message})).collect();
                    json!({"content": [{"type": "text", "text": json!({"schedules": list}).to_string()}]})
                }
                "delete" => {
                    let id = args["id"].as_str().unwrap_or("");
                    scheduler::delete_schedule(id);
                    json!({"content": [{"type": "text", "text": json!({"deleted": id}).to_string()}]})
                }
                "update" => {
                    let id = args["id"].as_str().unwrap_or("");
                    let enabled = args["enabled"].as_bool();
                    let cron = args["cron"].as_str();
                    let message = args["message"].as_str();
                    match scheduler::update_schedule(id, enabled, cron, message) {
                        Some(s) => {
                            json!({"content": [{"type": "text", "text": json!({"updated": s.id}).to_string()}]})
                        }
                        None => {
                            json!({"content": [{"type": "text", "text": "schedule not found"}], "isError": true})
                        }
                    }
                }
                _ => {
                    json!({"content": [{"type": "text", "text": format!("unknown schedule action: {action}")}], "isError": true})
                }
            }
        }
        "create_instance" => {
            let name = args["instance_name"]
                .as_str()
                .or_else(|| args["name"].as_str())
                .unwrap_or("");
            if name.is_empty() {
                return json!({"content": [{"type": "text", "text": "name required"}], "isError": true});
            }
            let backend = args["backend"].as_str().unwrap_or("claude");
            let model = args["model"].as_str();
            let wd = args["working_directory"]
                .as_str()
                .map(std::path::PathBuf::from);
            let branch = args["branch"].as_str().map(String::from);
            let mut cmd_parts = vec![backend.to_owned()];
            if let Some(m) = model {
                cmd_parts.push("--model".into());
                cmd_parts.push(m.into());
            }
            let command = cmd_parts.join(" ");
            let info = SpawnConfigInfo {
                command: command.clone(),
                working_dir: wd.clone(),
                worktree: true,
                branch: branch.clone(),
            };
            ctx.spawn_configs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(name.to_owned(), info);
            json!({"content": [{"type": "text", "text": json!({"created": name, "command": command, "branch": branch}).to_string()}]})
        }
        "replace_instance" => {
            let name = args["instance_name"]
                .as_str()
                .or_else(|| args["name"].as_str())
                .unwrap_or("");
            if name.is_empty() {
                return json!({"content": [{"type": "text", "text": "instance_name required"}], "isError": true});
            }
            // Check old instance exists
            let exists = ctx
                .writers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(name);
            if !exists {
                return json!({"content": [{"type": "text", "text": format!("instance '{name}' not found")}], "isError": true});
            }
            // Build new config (use provided or fall back to existing)
            let old_config = ctx
                .spawn_configs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(name)
                .cloned();
            let backend = args["backend"].as_str().unwrap_or("claude");
            let model = args["model"].as_str();
            let wd = args["working_directory"]
                .as_str()
                .map(std::path::PathBuf::from)
                .or_else(|| old_config.as_ref().and_then(|c| c.working_dir.clone()));
            let branch = args["branch"]
                .as_str()
                .map(String::from)
                .or_else(|| old_config.as_ref().and_then(|c| c.branch.clone()));
            let mut cmd_parts = vec![backend.to_owned()];
            if let Some(m) = model {
                cmd_parts.push("--model".into());
                cmd_parts.push(m.into());
            }
            let command = cmd_parts.join(" ");
            // Register new spawn config (create new)
            let info = SpawnConfigInfo {
                command: command.clone(),
                working_dir: wd.clone(),
                worktree: old_config.as_ref().map(|c| c.worktree).unwrap_or(true),
                branch: branch.clone(),
            };
            ctx.spawn_configs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(name.to_owned(), info);
            // Reset health monitor so respawn is guaranteed (even if previously Failed)
            if let Some(handle) = ctx
                .states
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(name)
            {
                if let Ok(mut h) = handle.health.lock() {
                    h.reset();
                }
            }
            // Kill old instance (Ctrl+C + EOF) — health monitor will respawn with new config
            if let Some(pw) = ctx
                .writers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(name)
            {
                let _ = pw
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .write_all(b"\x03\x04");
            }
            json!({"content": [{"type": "text", "text": json!({
                "replaced": name, "command": command, "branch": branch,
                "working_directory": wd.map(|p| p.display().to_string())
            }).to_string()}]})
        }
        _ => {
            json!({"content": [{"type": "text", "text": format!("unknown tool: {tool}")}], "isError": true})
        }
    }
}

fn inject_message(ctx: &DaemonCtx, sender: &str, target: &str, message: &str) -> ApiResponse {
    let w = ctx.writers.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(pw) = w.get(target) {
        // Use inbox for smart injection
        let text = match ctx.inbox.store_or_inject(target, sender, message, "\r") {
            crate::inbox::InjectAction::Direct(t) | crate::inbox::InjectAction::Notification(t) => {
                t
            }
        };
        match pw
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write_all(text.as_bytes())
        {
            Ok(_) => {
                tracing::info!(
                    sender = %sender,
                    target = %target,
                    preview = %message.chars().take(80).collect::<String>(),
                    "message injected"
                );
                ok(json!({"sent": true}))
            }
            Err(e) => err(format!("write: {e}")),
        }
    } else {
        err(format!("instance '{target}' not found"))
    }
}

pub fn mcp_tools_list() -> Value {
    json!({"tools": [
        {"name":"reply","description":"Reply to a Telegram user.","inputSchema":{"type":"object","properties":{"text":{"type":"string"},"format":{"type":"string","enum":["text","markdown","html"]},"reply_to":{"type":"string"}},"required":["text"]}},
        {"name":"send_to_instance","description":"Send a message to another agent instance.","inputSchema":{"type":"object","properties":{"instance_name":{"type":"string"},"message":{"type":"string"},"request_kind":{"type":"string","enum":["query","task","report","update"]},"requires_reply":{"type":"boolean"},"correlation_id":{"type":"string"}},"required":["instance_name","message"]}},
        {"name":"request_information","description":"Ask another agent a question.","inputSchema":{"type":"object","properties":{"instance_name":{"type":"string"},"question":{"type":"string"},"context":{"type":"string"}},"required":["instance_name","question"]}},
        {"name":"delegate_task","description":"Delegate a task to another agent.","inputSchema":{"type":"object","properties":{"instance_name":{"type":"string"},"task":{"type":"string"},"success_criteria":{"type":"string"},"context":{"type":"string"}},"required":["instance_name","task"]}},
        {"name":"report_result","description":"Report results back.","inputSchema":{"type":"object","properties":{"instance_name":{"type":"string"},"summary":{"type":"string"},"correlation_id":{"type":"string"},"artifacts":{"type":"string"}},"required":["instance_name","summary"]}},
        {"name":"broadcast","description":"Send to all agents (or team members).","inputSchema":{"type":"object","properties":{"message":{"type":"string"},"team":{"type":"string"}},"required":["message"]}},
        {"name":"list_instances","description":"List running agents.","inputSchema":{"type":"object","properties":{}}},
        {"name":"describe_instance","description":"Get agent details.","inputSchema":{"type":"object","properties":{"instance_name":{"type":"string"}},"required":["instance_name"]}},
        {"name":"delete_instance","description":"Stop an agent.","inputSchema":{"type":"object","properties":{"instance_name":{"type":"string"},"cleanup_worktree":{"type":"boolean"}},"required":["instance_name"]}},
        {"name":"inbox","description":"Read inbox messages.","inputSchema":{"type":"object","properties":{"id":{"type":"integer"}}}},
        {"name":"post_decision","description":"Post a fleet-wide decision.","inputSchema":{"type":"object","properties":{"title":{"type":"string"},"content":{"type":"string"}},"required":["title","content"]}},
        {"name":"list_decisions","description":"List fleet decisions.","inputSchema":{"type":"object","properties":{}}},
        {"name":"task","description":"Task board operations.","inputSchema":{"type":"object","properties":{"action":{"type":"string","enum":["create","list","claim","done","update"]},"title":{"type":"string"},"description":{"type":"string"},"id":{"type":"string"},"assignee":{"type":"string"},"status":{"type":"string","enum":["open","claimed","done","blocked"]},"result":{"type":"string"}},"required":["action"]}},
        {"name":"react","description":"React to a message with emoji.","inputSchema":{"type":"object","properties":{"message_id":{"type":"string"},"emoji":{"type":"string"}},"required":["message_id","emoji"]}},
        {"name":"edit_message","description":"Edit a sent message.","inputSchema":{"type":"object","properties":{"message_id":{"type":"string"},"text":{"type":"string"}},"required":["message_id","text"]}},
        {"name":"wait_for_idle","description":"Wait for an agent to become idle.","inputSchema":{"type":"object","properties":{"instance_name":{"type":"string"},"timeout_secs":{"type":"integer"}},"required":["instance_name"]}},
        {"name":"merge_preview","description":"Preview merge of agent branch.","inputSchema":{"type":"object","properties":{"instance_name":{"type":"string"}},"required":["instance_name"]}},
        {"name":"merge_agent","description":"Squash merge agent branch.","inputSchema":{"type":"object","properties":{"instance_name":{"type":"string"},"message":{"type":"string"}},"required":["instance_name"]}},
        {"name":"create_instance","description":"Create a new agent instance.","inputSchema":{"type":"object","properties":{"name":{"type":"string"},"working_directory":{"type":"string"},"backend":{"type":"string"},"model":{"type":"string"},"branch":{"type":"string"}},"required":["name"]}},
        {"name":"replace_instance","description":"Replace an agent with new settings (atomic swap).","inputSchema":{"type":"object","properties":{"instance_name":{"type":"string","description":"Agent to replace"},"backend":{"type":"string"},"model":{"type":"string"},"working_directory":{"type":"string"},"branch":{"type":"string"}},"required":["instance_name"]}},
        {"name":"update_decision","description":"Update a decision.","inputSchema":{"type":"object","properties":{"id":{"type":"integer"},"title":{"type":"string"},"content":{"type":"string"}},"required":["id"]}},
        {"name":"team","description":"Team operations.","inputSchema":{"type":"object","properties":{"action":{"type":"string","enum":["create","list","delete","update"]},"name":{"type":"string"},"members":{"type":"array","items":{"type":"string"}}},"required":["action"]}},
        {"name":"list_events","description":"List event log.","inputSchema":{"type":"object","properties":{"agent":{"type":"string"},"type":{"type":"string"}}}},
        {"name":"schedule","description":"Cron schedule operations.","inputSchema":{"type":"object","properties":{"action":{"type":"string","enum":["create","list","delete","update"]},"cron":{"type":"string"},"target":{"type":"string"},"message":{"type":"string"},"id":{"type":"string"},"enabled":{"type":"boolean"}},"required":["action"]}}
    ]})
}

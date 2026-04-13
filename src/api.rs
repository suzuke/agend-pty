//! API socket — JSON request/response for fleet management + MCP tool dispatch.
//!
//! Listens on ~/.agend/run/<pid>/api.sock
//! Protocol: newline-delimited JSON (one request per line, one response per line)

use crate::{channel, config, event_log, fleet_store, git, health, inbox, paths, scheduler, state};
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
    pub role: Option<String>,
}
pub type AgentStateMap = Arc<Mutex<HashMap<String, AgentStateHandle>>>;

/// Shared daemon context — holds all shared state.
/// Minimal spawn info for create_instance.
#[derive(Clone)]
pub struct SpawnConfigInfo {
    pub name: String,
    pub command: String,
    pub working_dir: Option<std::path::PathBuf>,
    pub worktree: bool,
    pub branch: Option<String>,
}

/// Persist a new/updated instance to fleet.yaml.
pub fn persist_to_fleet(ctx: &DaemonCtx, name: &str, info: &SpawnConfigInfo) {
    if let Some(ref path) = ctx.fleet_config_path {
        let ic = config::InstanceConfig {
            command: Some(info.command.clone()),
            working_directory: info.working_dir.clone(),
            worktree: Some(info.worktree),
            branch: info.branch.clone(),
            backend: None,
            model: None,
            skip_permissions: false,
            depends_on: vec![],
            max_session_hours: None,
            role: None,
        };
        if let Err(e) = config::FleetConfig::add_instance(path, name, ic) {
            tracing::warn!(name, error = %e, "failed to persist instance to fleet.yaml");
        }
    }
}

/// Remove an instance from fleet.yaml.
pub fn remove_from_fleet(ctx: &DaemonCtx, name: &str) {
    if let Some(ref path) = ctx.fleet_config_path {
        if let Err(e) = config::FleetConfig::remove_instance(path, name) {
            tracing::warn!(name, error = %e, "failed to remove instance from fleet.yaml");
        }
    }
}

/// Active CI watch entry.
#[derive(Clone)]
pub struct CiWatch {
    pub repo: String,
    pub pr: u64,
    pub on_failure: String,
    pub interval_secs: u64,
    pub last_check: u64,
}
pub type CiWatches = Arc<Mutex<Vec<CiWatch>>>;

pub struct DaemonCtx {
    pub writers: AgentWriters,
    pub states: AgentStateMap,
    pub spawn_configs: Arc<Mutex<HashMap<String, SpawnConfigInfo>>>,
    pub inbox: Arc<inbox::InboxStore>,
    pub channel_mgr: Arc<Mutex<channel::ChannelManager>>,
    /// Channel to request agent spawning from the daemon thread.
    pub spawn_tx: crossbeam::channel::Sender<SpawnConfigInfo>,
    pub ci_watches: CiWatches,
    /// Path to fleet.yaml — used to persist create/delete/replace instance changes.
    pub fleet_config_path: Option<std::path::PathBuf>,
    /// Names suppressed from respawn (deleted instances).
    pub deleted_names: Arc<Mutex<std::collections::HashSet<String>>>,
}

use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

static ACTIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);
const MAX_CONNECTIONS: usize = 64;

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
    // Restrict socket to owner only (0600)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)).ok();
    }
    tracing::info!(path = %sock.display(), "API listening");

    std::thread::Builder::new()
        .name("api_server".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                if ACTIVE_CONNECTIONS.load(AtomicOrdering::Relaxed) >= MAX_CONNECTIONS {
                    tracing::warn!("max API connections reached, rejecting");
                    drop(stream);
                    continue;
                }
                ACTIVE_CONNECTIONS.fetch_add(1, AtomicOrdering::Relaxed);
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
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            line.clear();
                            continue;
                        }
                        // Try to parse as JSON
                        let resp = match serde_json::from_str::<ApiRequest>(trimmed) {
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
                    ACTIVE_CONNECTIONS.fetch_sub(1, AtomicOrdering::Relaxed);
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
            let writers = ctx.writers.lock().unwrap_or_else(|e| e.into_inner());
            let states = ctx.states.lock().unwrap_or_else(|e| e.into_inner());
            let agents: Vec<Value> = writers
                .keys()
                .map(|n| {
                    let st = states
                        .get(n)
                        .and_then(|h| {
                            h.state_machine
                                .lock()
                                .ok()
                                .map(|s| format!("{:?}", s.state()))
                        })
                        .unwrap_or_else(|| "Unknown".into());
                    let hl = states
                        .get(n)
                        .and_then(|h| h.health.lock().ok().map(|hm| format!("{:?}", hm.status())))
                        .unwrap_or_else(|| "Unknown".into());
                    json!({"name": n, "state": st, "health": hl})
                })
                .collect();
            ok(json!({"agents": agents}))
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

        _ => err(format!("unknown method: {}", req.method)),
    }
}

/// MCP tool dispatch — routes tool calls to handlers.
/// Organized by category: communication, fleet, coordination, git, CI.
fn handle_mcp_tool(ctx: &DaemonCtx, instance: &str, tool: &str, args: &Value) -> Value {
    match tool {
        // ── Communication ──
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
        // ── Fleet management ──
        "delete_instance" => {
            let name = args["instance_name"]
                .as_str()
                .or_else(|| args["name"].as_str())
                .unwrap_or("");
            let cleanup_wt = args["cleanup_worktree"].as_bool().unwrap_or(false);
            // Single lock scope: check existence, then count guard
            let w = ctx.writers.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(pw) = w.get(name) {
                if w.len() <= 1 {
                    return json!({"content": [{"type": "text", "text": "cannot delete the last running instance"}], "isError": true});
                }
                // 1. Suppress respawn BEFORE killing
                ctx.deleted_names
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(name.to_owned());
                ctx.spawn_configs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(name);
                // 2. Now kill
                let _ = pw
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .write_all(b"\x03\x04");
                drop(w);
                remove_from_fleet(ctx, name);
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
        // ── Coordination ──
        "decision" => {
            let action = args["action"].as_str().unwrap_or("");
            match action {
                "post" => {
                    let title = args["title"].as_str().unwrap_or("");
                    let content = args["content"].as_str().unwrap_or("");
                    let d = fleet_store::post_decision(instance, title, content);
                    json!({"content": [{"type": "text", "text": json!({"posted": true, "id": d.id}).to_string()}]})
                }
                "list" => {
                    let decisions = fleet_store::list_decisions();
                    let list: Vec<Value> = decisions
                        .iter()
                        .map(|d| json!({"id": d.id, "title": d.title, "author": d.author}))
                        .collect();
                    json!({"content": [{"type": "text", "text": json!({"decisions": list}).to_string()}]})
                }
                "update" => {
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
                _ => {
                    json!({"content": [{"type": "text", "text": format!("unknown decision action: {action}")}], "isError": true})
                }
            }
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
            // Note: blocks this API handler thread (each connection has its own thread)
            let target = args["instance_name"].as_str().unwrap_or("");
            let timeout = args["timeout_secs"].as_u64().unwrap_or(60).min(120);
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
        // ── Git integration ──
        "merge" => {
            let action = args["action"].as_str().unwrap_or("");
            match action {
                "preview" => {
                    let target = args["instance_name"].as_str().unwrap_or(instance);
                    let branch = format!("agend/{target}");
                    let repo = ctx
                        .states
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(target)
                        .and_then(|h| h.working_dir.clone());
                    match repo {
                        Some(p) => match git::merge_preview(&p, &branch) {
                            Ok(p) => {
                                json!({"content": [{"type": "text", "text": json!({"diff_stat": p.diff_stat, "files_changed": p.files_changed, "has_conflicts": p.has_conflicts}).to_string()}]})
                            }
                            Err(e) => {
                                json!({"content": [{"type": "text", "text": e}], "isError": true})
                            }
                        },
                        None => {
                            json!({"content": [{"type": "text", "text": format!("instance '{target}' not found")}], "isError": true})
                        }
                    }
                }
                "squash" => {
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
                    match repo {
                        Some(p) => match git::squash_merge(&p, &branch, message) {
                            Ok(()) => {
                                json!({"content": [{"type": "text", "text": "{\"merged\":true}"}]})
                            }
                            Err(e) => {
                                json!({"content": [{"type": "text", "text": e}], "isError": true})
                            }
                        },
                        None => {
                            json!({"content": [{"type": "text", "text": format!("instance '{target}' not found")}], "isError": true})
                        }
                    }
                }
                "all" => {
                    let prefix = args["message"].as_str().unwrap_or("merge");
                    let states = ctx.states.lock().unwrap_or_else(|e| e.into_inner());
                    let mut results: Vec<Value> = Vec::new();
                    for (name, handle) in states.iter() {
                        if let Some(ref wd) = handle.working_dir {
                            let branch = format!("agend/{name}");
                            let msg = format!("{prefix} {name}");
                            match git::squash_merge(wd, &branch, &msg) {
                                Ok(()) => results.push(json!({"agent": name, "merged": true})),
                                Err(e) => results.push(json!({"agent": name, "error": e})),
                            }
                        }
                    }
                    json!({"content": [{"type": "text", "text": json!({"results": results}).to_string()}]})
                }
                _ => {
                    json!({"content": [{"type": "text", "text": format!("unknown merge action: {action}")}], "isError": true})
                }
            }
        }
        // Legacy aliases for backward compat
        "post_decision" | "list_decisions" | "update_decision" | "merge_preview"
        | "merge_agent" | "merge_all" => {
            json!({"content": [{"type": "text", "text": format!("'{tool}' is deprecated. Use 'decision' or 'merge' with action parameter.")}], "isError": true})
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
        "start_instance" => {
            let name = args["instance_name"]
                .as_str()
                .or_else(|| args["name"].as_str())
                .unwrap_or("");
            if name.is_empty() {
                return json!({"content": [{"type": "text", "text": "instance_name required"}], "isError": true});
            }
            // Already running?
            let running = ctx
                .writers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(name);
            if running {
                return json!({"content": [{"type": "text", "text": format!("instance '{name}' already running")}], "isError": true});
            }
            // Reset health so it doesn't block
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
            // Actually spawn via the spawn channel
            let config = ctx
                .spawn_configs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(name)
                .cloned();
            if let Some(cfg) = config {
                match ctx.spawn_tx.send(SpawnConfigInfo {
                    name: name.to_owned(),
                    command: cfg.command,
                    working_dir: cfg.working_dir,
                    worktree: cfg.worktree,
                    branch: cfg.branch,
                }) {
                    Ok(()) => {
                        json!({"content": [{"type": "text", "text": json!({"started": name}).to_string()}]})
                    }
                    Err(e) => {
                        json!({"content": [{"type": "text", "text": format!("spawn failed: {e}")}], "isError": true})
                    }
                }
            } else {
                json!({"content": [{"type": "text", "text": format!("no config found for '{name}'. Use create_instance first.")}], "isError": true})
            }
        }
        "create_instance" => {
            let name = args["instance_name"]
                .as_str()
                .or_else(|| args["name"].as_str())
                .unwrap_or("");
            let name = &crate::util::sanitize_name(name);
            if name.is_empty() {
                return json!({"content": [{"type": "text", "text": "name required (alphanumeric, hyphens, underscores only)"}], "isError": true});
            }
            let backend_str = args["backend"].as_str().unwrap_or("claude");
            let resolved = crate::config::resolve_backend_binary(backend_str);
            let model = args["model"].as_str();
            let wd = args["working_directory"]
                .as_str()
                .map(std::path::PathBuf::from);
            let branch = args["branch"].as_str().map(String::from);
            // Build command with preset args (e.g. --dangerously-skip-permissions)
            let mut cmd_parts = vec![resolved.clone()];
            if let Some(b) = crate::backend::Backend::from_command(&resolved) {
                for arg in b.preset().args {
                    cmd_parts.push(arg.to_string());
                }
            }
            if let Some(m) = model {
                cmd_parts.push("--model".into());
                cmd_parts.push(m.into());
            }
            let command = cmd_parts.join(" ");
            let info = SpawnConfigInfo {
                name: name.to_owned(),
                command: command.clone(),
                working_dir: wd.clone(),
                worktree: true,
                branch: branch.clone(),
            };
            // Persist to fleet.yaml for daemon restart
            persist_to_fleet(ctx, name, &info);
            // Send spawn request to daemon thread (which has access to registry)
            match ctx.spawn_tx.send(info) {
                Ok(()) => {
                    json!({"content": [{"type": "text", "text": json!({"created": name, "command": command, "branch": branch}).to_string()}]})
                }
                Err(e) => {
                    json!({"content": [{"type": "text", "text": format!("spawn failed: {e}")}], "isError": true})
                }
            }
        }
        "replace_instance" => {
            let name = args["instance_name"]
                .as_str()
                .or_else(|| args["name"].as_str())
                .unwrap_or("");
            if name.is_empty() {
                return json!({"content": [{"type": "text", "text": "instance_name required"}], "isError": true});
            }
            // Check existence (actual kill happens below in same-scope pattern)
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
            let info = SpawnConfigInfo {
                name: name.to_owned(),
                command: command.clone(),
                working_dir: wd.clone(),
                worktree: old_config.as_ref().map(|c| c.worktree).unwrap_or(true),
                branch: branch.clone(),
            };
            // Store config for respawn after kill + persist to fleet.yaml
            persist_to_fleet(ctx, name, &info);
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
            // Kill old instance atomically (check + kill in same lock scope)
            {
                let w = ctx.writers.lock().unwrap_or_else(|e| e.into_inner());
                match w.get(name) {
                    Some(pw) => {
                        let _ = pw
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .write_all(b"\x03\x04");
                    }
                    None => {
                        return json!({"content": [{"type": "text", "text": format!("instance '{name}' not found")}], "isError": true});
                    }
                }
            }
            json!({"content": [{"type": "text", "text": json!({
                "replaced": name, "command": command, "branch": branch,
                "working_directory": wd.map(|p| p.display().to_string())
            }).to_string()}]})
        }
        // ── CI ──
        "watch_ci" => {
            let repo = args["repo"].as_str().unwrap_or("");
            let pr = args["pr"].as_u64().unwrap_or(0);
            if repo.is_empty() || pr == 0 {
                return json!({"content": [{"type": "text", "text": "repo and pr required"}], "isError": true});
            }
            if !repo.contains('/') || repo.contains(' ') || repo.starts_with('-') {
                return json!({"content": [{"type": "text", "text": "invalid repo format, expected: owner/repo"}], "isError": true});
            }
            let on_failure = args["on_failure"].as_str().unwrap_or(instance).to_owned();
            let interval = args["interval_secs"].as_u64().unwrap_or(60).max(30);
            // Do an immediate check first
            let status = check_ci_status(repo, pr);
            // Register persistent watch
            ctx.ci_watches
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(CiWatch {
                    repo: repo.to_owned(),
                    pr,
                    on_failure,
                    interval_secs: interval,
                    last_check: crate::util::now_secs(),
                });
            json!({"content": [{"type": "text", "text": json!({"watching": true, "repo": repo, "pr": pr, "interval": interval, "current_status": status}).to_string()}]})
        }
        "unwatch_ci" => {
            let repo = args["repo"].as_str().unwrap_or("");
            let pr = args["pr"].as_u64().unwrap_or(0);
            let mut watches = ctx.ci_watches.lock().unwrap_or_else(|e| e.into_inner());
            let before = watches.len();
            watches.retain(|w| !(w.repo == repo && w.pr == pr));
            let removed = before - watches.len();
            json!({"content": [{"type": "text", "text": json!({"unwatched": removed > 0, "repo": repo, "pr": pr}).to_string()}]})
        }
        _ => {
            json!({"content": [{"type": "text", "text": format!("unknown tool: {tool}")}], "isError": true})
        }
    }
}

pub fn inject_message(ctx: &DaemonCtx, sender: &str, target: &str, message: &str) -> ApiResponse {
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

/// Check CI status for a PR via `gh` CLI.
pub fn check_ci_status(repo: &str, pr: u64) -> Value {
    let output = std::process::Command::new("gh")
        .args([
            "pr",
            "checks",
            &pr.to_string(),
            "--repo",
            repo,
            "--json",
            "name,status,conclusion",
        ])
        .output();
    match output {
        Ok(o) if o.status.success() => {
            let body = String::from_utf8_lossy(&o.stdout);
            let checks: Value = serde_json::from_str(body.trim()).unwrap_or(json!([]));
            let empty = vec![];
            let arr = checks.as_array().unwrap_or(&empty);
            let failures: Vec<&str> = arr
                .iter()
                .filter(|c| c["conclusion"].as_str() == Some("failure"))
                .filter_map(|c| c["name"].as_str())
                .collect();
            json!({"checks": arr.len(), "failures": failures})
        }
        Ok(o) => json!({"error": String::from_utf8_lossy(&o.stderr).trim().to_string()}),
        Err(e) => json!({"error": format!("gh not found: {e}")}),
    }
}

/// Called from daemon tick thread — poll all active CI watches.
pub fn tick_ci_watches(ctx: &DaemonCtx) {
    let now = crate::util::now_secs();
    let due: Vec<(String, u64, String)> = {
        let mut watches = ctx.ci_watches.lock().unwrap_or_else(|e| e.into_inner());
        let mut result = Vec::new();
        for watch in watches.iter_mut() {
            if now.saturating_sub(watch.last_check) >= watch.interval_secs {
                watch.last_check = now;
                result.push((watch.repo.clone(), watch.pr, watch.on_failure.clone()));
            }
        }
        result
    };
    for (repo, pr, on_failure) in &due {
        let status = check_ci_status(repo, *pr);
        if let Some(failures) = status["failures"].as_array() {
            if !failures.is_empty() {
                let names: Vec<&str> = failures.iter().filter_map(|f| f.as_str()).collect();
                let msg = format!(
                    "[CI] {repo}#{pr}: {} failed: {}",
                    names.len(),
                    names.join(", ")
                );
                inject_message(ctx, "ci-watch", on_failure, &msg);
            }
        }
    }
}

// ── Extracted tool handlers (testable without DaemonCtx) ────────────────

pub fn handle_decision_tool(
    action: &str,
    instance: &str,
    args: &serde_json::Value,
) -> serde_json::Value {
    match action {
        "post" => {
            let title = args["title"].as_str().unwrap_or("");
            let content = args["content"].as_str().unwrap_or("");
            let d = fleet_store::post_decision(instance, title, content);
            json!({"content": [{"type": "text", "text": json!({"posted": true, "id": d.id}).to_string()}]})
        }
        "list" => {
            let decisions = fleet_store::list_decisions();
            let list: Vec<serde_json::Value> = decisions
                .iter()
                .map(|d| json!({"id": d.id, "title": d.title, "author": d.author}))
                .collect();
            json!({"content": [{"type": "text", "text": json!({"decisions": list}).to_string()}]})
        }
        _ => {
            json!({"content": [{"type": "text", "text": format!("unknown decision action: {action}")}], "isError": true})
        }
    }
}

pub fn handle_task_tool(
    action: &str,
    instance: &str,
    args: &serde_json::Value,
) -> serde_json::Value {
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
            let list: Vec<serde_json::Value> = tasks.iter().map(|t| json!({"id": t.id, "title": t.title, "status": t.status, "assignee": t.assignee})).collect();
            json!({"content": [{"type": "text", "text": json!({"tasks": list}).to_string()}]})
        }
        _ => {
            json!({"content": [{"type": "text", "text": format!("unknown task action: {action}")}], "isError": true})
        }
    }
}

pub fn handle_team_tool(action: &str, args: &serde_json::Value) -> serde_json::Value {
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
            let list: Vec<serde_json::Value> = teams
                .iter()
                .map(|t| json!({"name": t.name, "members": t.members}))
                .collect();
            json!({"content": [{"type": "text", "text": json!({"teams": list}).to_string()}]})
        }
        _ => {
            json!({"content": [{"type": "text", "text": format!("unknown team action: {action}")}], "isError": true})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_post_returns_id() {
        let r = handle_decision_tool(
            "post",
            "alice",
            &json!({"title": "test", "content": "body"}),
        );
        let text = r["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("\"posted\":true"));
    }

    #[test]
    fn decision_unknown_action_errors() {
        let r = handle_decision_tool("bad", "a", &json!({}));
        assert!(r["isError"].as_bool() == Some(true));
    }

    #[test]
    fn task_create_returns_id() {
        let r = handle_task_tool("create", "bob", &json!({"title": "fix bug"}));
        let text = r["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("\"created\""));
    }

    #[test]
    fn task_unknown_action_errors() {
        let r = handle_task_tool("bad", "a", &json!({}));
        assert!(r["isError"].as_bool() == Some(true));
    }

    #[test]
    fn team_create_returns_name() {
        let r = handle_team_tool("create", &json!({"name": "core", "members": ["a", "b"]}));
        let text = r["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("core"));
    }

    #[test]
    fn team_unknown_action_errors() {
        let r = handle_team_tool("bad", &json!({}));
        assert!(r["isError"].as_bool() == Some(true));
    }
}

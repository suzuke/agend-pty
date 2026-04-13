//! Shared operations — business logic extracted from MCP tool handlers.
//! Called by handle_mcp_tool (server-side). CLI uses API socket to reach these.

use crate::{api::DaemonCtx, event_log, fleet_store, git, scheduler, state};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

// ── Communication ───────────────────────────────────────────────────────

pub fn send_message(ctx: &DaemonCtx, from: &str, target: &str, message: &str) -> Value {
    match crate::api::inject_message(ctx, from, target, message) {
        crate::api::ApiResponse { ok: true, .. } => json!({"sent": true, "target": target}),
        crate::api::ApiResponse { error: Some(e), .. } => json!({"error": e}),
        _ => json!({"error": "unknown error"}),
    }
}

pub fn broadcast(ctx: &DaemonCtx, from: &str, message: &str, team: Option<&str>) -> Value {
    let team_members = team.and_then(fleet_store::get_team_members);
    let names: Vec<String> = ctx
        .writers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .filter(|k| *k != from)
        .filter(|k| team_members.as_ref().map(|m| m.contains(k)).unwrap_or(true))
        .cloned()
        .collect();
    for target in &names {
        crate::api::inject_message(ctx, from, target, message);
    }
    let skipped: Vec<String> = team_members
        .as_ref()
        .map(|m| {
            m.iter()
                .filter(|k| !names.contains(k) && k.as_str() != from)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    json!({"broadcast": true, "sent_to": names, "skipped": skipped})
}

pub fn list_instances(ctx: &DaemonCtx) -> Value {
    let names: Vec<String> = ctx
        .writers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .cloned()
        .collect();
    json!({"instances": names})
}

pub fn describe_instance(ctx: &DaemonCtx, name: &str) -> Value {
    let w = ctx.writers.lock().unwrap_or_else(|e| e.into_inner());
    if w.contains_key(name) {
        json!({"name": name, "status": "running"})
    } else {
        json!({"error": format!("instance '{name}' not found")})
    }
}

pub fn reply(
    ctx: &DaemonCtx,
    instance: &str,
    text: &str,
    format_mode: &str,
    reply_to: Option<&str>,
) -> Value {
    let formatted = format!("[{instance}] {text}");
    let msg_id = ctx
        .channel_mgr
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .send_to_agent_ext(instance, &formatted, format_mode, reply_to);
    json!({"replied": true, "message_id": msg_id.unwrap_or_default()})
}

pub fn inbox_list(ctx: &DaemonCtx, instance: &str) -> Value {
    let msgs = ctx.inbox.list(instance);
    let list: Vec<Value> = msgs
        .iter()
        .map(|m| {
            json!({"id": m.id, "sender": m.sender, "preview": m.text.chars().take(100).collect::<String>()})
        })
        .collect();
    json!({"messages": list})
}

pub fn inbox_get(ctx: &DaemonCtx, instance: &str, id: u64) -> Value {
    match ctx.inbox.get(instance, id) {
        Some(msg) => json!({"sender": msg.sender, "text": msg.text}),
        None => json!({"error": "message not found"}),
    }
}

// ── Fleet management ────────────────────────────────────────────────────

pub fn delete_instance(ctx: &DaemonCtx, name: &str, cleanup_wt: bool) -> Value {
    let w = ctx.writers.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(pw) = w.get(name) {
        if w.len() <= 1 {
            return json!({"error": "cannot delete the last running instance"});
        }
        let saved_config = ctx
            .spawn_configs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned();
        ctx.deleted_names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_owned());
        ctx.spawn_configs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        let _ = pw
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write_all(b"\x03\x04");
        drop(w);
        crate::api::remove_from_fleet(ctx, name);
        if let Some(ref cfg) = saved_config {
            if let Some(ref wd) = cfg.working_dir {
                crate::mcp_config::remove_mcp_config(wd, &cfg.command, name);
            }
        }
        let mut resp = json!({"deleted": name});
        if cleanup_wt {
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
        resp
    } else {
        json!({"error": format!("instance '{name}' not found")})
    }
}

pub fn create_instance(ctx: &DaemonCtx, args: &Value) -> Value {
    let name = args["name"].as_str().unwrap_or("");
    let name = &crate::util::sanitize_name(name);
    if name.is_empty() {
        return json!({"error": "name required"});
    }
    let backend_str = args["backend"].as_str().unwrap_or("claude");
    let resolved = crate::config::resolve_backend_binary(backend_str);
    let model = args["model"].as_str();
    let wd = args["working_directory"]
        .as_str()
        .map(std::path::PathBuf::from);
    let branch = args["branch"].as_str().map(String::from);
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
    let info = crate::api::SpawnConfigInfo {
        name: name.to_owned(),
        command: command.clone(),
        working_dir: wd,
        worktree: true,
        branch: branch.clone(),
    };
    crate::api::persist_to_fleet(ctx, name, &info);
    match ctx.spawn_tx.send(info) {
        Ok(()) => json!({"created": name, "command": command, "branch": branch}),
        Err(e) => json!({"error": format!("spawn failed: {e}")}),
    }
}

pub fn start_instance(ctx: &DaemonCtx, name: &str) -> Value {
    if name.is_empty() {
        return json!({"error": "instance_name required"});
    }
    let running = ctx
        .writers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(name);
    if running {
        return json!({"error": format!("instance '{name}' already running")});
    }
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
    let config = ctx
        .spawn_configs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(name)
        .cloned();
    if let Some(cfg) = config {
        match ctx.spawn_tx.send(crate::api::SpawnConfigInfo {
            name: name.to_owned(),
            command: cfg.command,
            working_dir: cfg.working_dir,
            worktree: cfg.worktree,
            branch: cfg.branch,
        }) {
            Ok(()) => json!({"started": name}),
            Err(e) => json!({"error": format!("spawn failed: {e}")}),
        }
    } else {
        json!({"error": format!("no config found for '{name}'")})
    }
}

// ── Coordination ────────────────────────────────────────────────────────

pub fn decision(instance: &str, action: &str, args: &Value) -> Value {
    match action {
        "post" => {
            let title = args["title"].as_str().unwrap_or("");
            let content = args["content"].as_str().unwrap_or("");
            let d = fleet_store::post_decision(instance, title, content);
            json!({"posted": true, "id": d.id})
        }
        "list" => {
            let decisions = fleet_store::list_decisions();
            let list: Vec<Value> = decisions
                .iter()
                .map(|d| json!({"id": d.id, "title": d.title, "author": d.author}))
                .collect();
            json!({"decisions": list})
        }
        "update" => {
            let id = args["id"].as_u64().unwrap_or(0);
            let title = args["title"].as_str();
            let content = args["content"].as_str();
            match fleet_store::update_decision(id, title, content) {
                Some(d) => json!({"updated": d.id}),
                None => json!({"error": "decision not found"}),
            }
        }
        _ => json!({"error": format!("unknown decision action: {action}")}),
    }
}

pub fn task(instance: &str, action: &str, args: &Value) -> Value {
    match action {
        "create" => {
            let title = args["title"].as_str().unwrap_or("untitled");
            let desc = args["description"].as_str().unwrap_or("");
            let assignee = args["assignee"].as_str().unwrap_or("");
            let t = fleet_store::create_task(instance, title, desc, assignee);
            json!({"created": t.id})
        }
        "list" => {
            let tasks = fleet_store::list_tasks();
            let list: Vec<Value> = tasks
                .iter()
                .map(|t| json!({"id": t.id, "title": t.title, "status": t.status, "assignee": t.assignee}))
                .collect();
            json!({"tasks": list})
        }
        "claim" => {
            let id = args["id"].as_str().unwrap_or("");
            match fleet_store::update_task(id, Some("claimed"), Some(instance), None) {
                Some(t) => json!({"claimed": t.id}),
                None => json!({"error": "task not found"}),
            }
        }
        "done" => {
            let id = args["id"].as_str().unwrap_or("");
            let result = args["result"].as_str().unwrap_or("");
            match fleet_store::update_task(id, Some("done"), None, Some(result)) {
                Some(t) => json!({"done": t.id}),
                None => json!({"error": "task not found"}),
            }
        }
        "update" => {
            let id = args["id"].as_str().unwrap_or("");
            let status = args["status"].as_str();
            let assignee = args["assignee"].as_str();
            match fleet_store::update_task(id, status, assignee, None) {
                Some(t) => json!({"updated": t.id}),
                None => json!({"error": "task not found"}),
            }
        }
        _ => json!({"error": format!("unknown task action: {action}")}),
    }
}

pub fn team(action: &str, args: &Value) -> Value {
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
            json!({"created": t.name})
        }
        "list" => {
            let teams = fleet_store::list_teams();
            let list: Vec<Value> = teams
                .iter()
                .map(|t| json!({"name": t.name, "members": t.members}))
                .collect();
            json!({"teams": list})
        }
        "delete" => {
            let name = args["name"].as_str().unwrap_or("");
            fleet_store::delete_team(name);
            json!({"deleted": name})
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
                Some(t) => json!({"updated": t.name}),
                None => json!({"error": "team not found"}),
            }
        }
        _ => json!({"error": format!("unknown team action: {action}")}),
    }
}

pub fn schedule(action: &str, args: &Value) -> Value {
    match action {
        "create" => {
            let cron = args["cron"].as_str().unwrap_or("* * * * *");
            let target = args["target"].as_str().unwrap_or("");
            let message = args["message"].as_str().unwrap_or("");
            match scheduler::create_schedule(cron, target, message) {
                Ok(s) => json!({"created": s.id}),
                Err(e) => json!({"error": e}),
            }
        }
        "list" => {
            let schedules = scheduler::list_schedules();
            let list: Vec<Value> = schedules
                .iter()
                .map(|s| json!({"id": s.id, "cron": s.cron, "target": s.target, "message": s.message}))
                .collect();
            json!({"schedules": list})
        }
        "delete" => {
            let id = args["id"].as_str().unwrap_or("");
            scheduler::delete_schedule(id);
            json!({"deleted": id})
        }
        "update" => {
            let id = args["id"].as_str().unwrap_or("");
            let enabled = args["enabled"].as_bool();
            let cron = args["cron"].as_str();
            let message = args["message"].as_str();
            match scheduler::update_schedule(id, enabled, cron, message) {
                Some(s) => json!({"updated": s.id}),
                None => json!({"error": "schedule not found"}),
            }
        }
        _ => json!({"error": format!("unknown schedule action: {action}")}),
    }
}

pub fn list_events(agent: Option<&str>, etype: Option<&str>) -> Value {
    let events = event_log::list_events(agent, etype);
    let list: Vec<Value> = events
        .iter()
        .map(|e| json!({"ts": e.ts, "type": e.event_type, "agent": e.agent, "details": e.details}))
        .collect();
    json!({"events": list})
}

pub fn react(ctx: &DaemonCtx, instance: &str, message_id: &str, emoji: &str) -> Value {
    match ctx
        .channel_mgr
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .react(instance, message_id, emoji)
    {
        Ok(()) => json!({"reacted": true}),
        Err(e) => json!({"error": e}),
    }
}

pub fn edit_message(ctx: &DaemonCtx, instance: &str, message_id: &str, text: &str) -> Value {
    match ctx
        .channel_mgr
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .edit_message(instance, message_id, text)
    {
        Ok(()) => json!({"edited": true}),
        Err(e) => json!({"error": e}),
    }
}

pub fn wait_for_idle(ctx: &DaemonCtx, target: &str, timeout: u64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(timeout.min(120));
    loop {
        let agent_state = ctx
            .states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(target)
            .and_then(|h| h.state_machine.lock().ok().map(|s| s.state()));
        match agent_state {
            Some(s @ (state::AgentState::Ready | state::AgentState::Idle)) => {
                return json!({"idle": true, "state": format!("{s:?}")});
            }
            Some(s @ (state::AgentState::Crashed | state::AgentState::Errored)) => {
                return json!({"error": format!("agent '{target}' is {s:?}")});
            }
            None => {
                return json!({"error": format!("instance '{target}' not found")});
            }
            _ => {}
        }
        if Instant::now() > deadline {
            return json!({"error": format!("timeout after {timeout}s waiting for '{target}'")});
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

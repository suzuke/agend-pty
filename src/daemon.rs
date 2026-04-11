//! agend-daemon: multi-agent PTY manager + MCP server.
//!
//! Usage: agend-daemon [name:command ...]
//!
//! Improvements over initial POC:
//! 1. crossbeam broadcast channel for output distribution
//! 2. alacritty_terminal VTerm for screen state (reconnect gets proper screen dump)
//! 3. Atomic subscribe+dump (no output gap on reconnect)

#[path = "config.rs"]
mod config;
#[path = "vterm.rs"]
mod vterm;
#[path = "backend.rs"]
mod backend;
#[path = "instructions.rs"]
mod instructions;
#[path = "paths.rs"]
mod paths;
#[path = "api.rs"]
mod api;
#[path = "telegram.rs"]
mod telegram;
#[path = "inbox.rs"]
mod inbox;
#[path = "doctor.rs"]
mod doctor;
#[path = "mcp_config.rs"]
mod mcp_config;
#[path = "channel.rs"]
mod channel;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use channel::ChannelAdapter;

// ── Framing ─────────────────────────────────────────────────────────────
// Protocol: [u8 tag][u32 BE len][bytes]
// Tag 0 = PTY data, Tag 1 = resize (4 bytes: cols_hi, cols_lo, rows_hi, rows_lo)

const TAG_DATA: u8 = 0;
const TAG_RESIZE: u8 = 1;

fn write_frame(w: &mut impl Write, data: &[u8]) -> std::io::Result<()> {
    w.write_all(&[TAG_DATA])?;
    w.write_all(&(data.len() as u32).to_be_bytes())?;
    w.write_all(data)?;
    w.flush()
}

fn read_tagged_frame(r: &mut impl Read) -> std::io::Result<(u8, Vec<u8>)> {
    let mut tag = [0u8; 1];
    r.read_exact(&mut tag)?;
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 1_000_000 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok((tag[0], buf))
}

// ── ANSI stripping (for dialog detection) ───────────────────────────────

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    while let Some(&ch) = chars.peek() {
                        chars.next();
                        if ch.is_ascii_alphabetic() {
                            if ch == 'C' || ch == 'D' { out.push(' '); }
                            break;
                        }
                    }
                }
                Some(']') => { chars.next(); while let Some(&ch) = chars.peek() { chars.next(); if ch == '\x07' || ch == '\\' { break; } } }
                Some('(') | Some(')') => { chars.next(); chars.next(); }
                _ => { chars.next(); }
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ── Shared agent state ──────────────────────────────────────────────────

type PtyWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// Core state for one agent — protected by a single Mutex for atomic operations.
struct AgentCore {
    vterm: vterm::VTerm,
    /// Output subscribers — each gets its own unbounded channel.
    /// Dead subscribers auto-removed on send failure.
    subscribers: Vec<crossbeam::channel::Sender<Vec<u8>>>,
}

impl AgentCore {
    fn broadcast(&mut self, data: &[u8]) {
        self.subscribers.retain(|tx| tx.send(data.to_vec()).is_ok());
    }

    fn subscribe(&mut self) -> crossbeam::channel::Receiver<Vec<u8>> {
        let (tx, rx) = crossbeam::channel::unbounded();
        self.subscribers.push(tx);
        rx
    }
}

struct AgentHandle {
    pty_writer: PtyWriter,
    core: Arc<Mutex<AgentCore>>,
    submit_key: String,
    inject_prefix: String,
    typed_inject: bool,
}

type AgentRegistry = Arc<Mutex<HashMap<String, AgentHandle>>>;

fn socket_path(name: &str) -> std::path::PathBuf { paths::tui_socket(name) }
fn mcp_socket_path(name: &str) -> std::path::PathBuf { paths::mcp_socket(name) }

// ── Backend MCP injection ────────────────────────────────────────────────

/// Inject MCP config into the command based on the backend type.
fn inject_mcp_for_backend(command: &str, name: &str, mcp_config_path: &str, prompt_path: &str) -> String {
    let bin = command.split_whitespace().next().unwrap_or(command);
    match bin {
        "claude" => format!("{command} --mcp-config {mcp_config_path} --append-system-prompt-file {prompt_path}"),
        "gemini" => {
            // Gemini reads .gemini/settings.json from working dir — write MCP there
            // For now, pass via command line isn't supported, so we rely on config file
            // The MCP bridge config is written to working_dir/.gemini/settings.json by the caller
            command.to_owned()
        }
        "kiro-cli" => format!("{command}"),  // kiro reads .kiro/settings/mcp.json
        "codex" => command.to_owned(),       // codex uses `codex mcp add`
        _ => command.to_owned(),             // unknown backend — no injection
    }
}

// ── Agent spawning ──────────────────────────────────────────────────────

fn spawn_agent(name: String, command: String, working_dir: Option<std::path::PathBuf>, registry: AgentRegistry, agent_writers: api::AgentWriters, inbox_store: Arc<inbox::InboxStore>, channel_mgr: Arc<Mutex<channel::ChannelManager>>) {
    let sock = socket_path(&name);
    let _ = std::fs::remove_file(&sock);

    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .unwrap_or_else(|e| panic!("[{name}] failed to open pty: {e}"));

    // Inject MCP config based on backend
    let bridge_path = std::env::current_exe()
        .map(|p| p.parent().unwrap().join("agend-mcp-bridge").to_string_lossy().into_owned())
        .unwrap_or_else(|_| "agend-mcp-bridge".into());
    let mcp_config_path = paths::agent_dir(&name).join("mcp-config.json");
    let mcp_config_path_str = mcp_config_path.display().to_string();
    let mcp_config = serde_json::json!({
        "mcpServers": { format!("agend-{name}"): {
            "command": bridge_path,
            "args": ["--socket", paths::mcp_socket(&name).to_str().expect("non-UTF8 path")]
        } }
    });
    std::fs::write(&mcp_config_path, serde_json::to_string_pretty(&mcp_config).unwrap()).ok();

    // System prompt for fleet awareness
    let prompt_path = paths::agent_dir(&name).join("prompt.md");
    let prompt_path_str = prompt_path.display().to_string();
    let other_agents: Vec<String> = registry.lock().unwrap_or_else(|e| e.into_inner()).keys()
        .filter(|k| *k != &name).cloned().collect();
    let prompt = format!(
        "You are '{}', part of an AI agent fleet.\nOther agents: {}\n\
         You have `send_to_instance` and `list_instances` MCP tools.\n\
         When you receive a [message from X], respond directly. \
         If a reply is needed, use send_to_instance. Do NOT ask permission.",
        name, if other_agents.is_empty() { "(none yet)".into() } else { other_agents.join(", ") }
    );
    std::fs::write(&prompt_path, &prompt).ok();

    // Build final command with backend-specific MCP injection
    let final_command = inject_mcp_for_backend(&command, &name, &mcp_config_path_str, &prompt_path_str);

    let parts: Vec<&str> = final_command.split_whitespace().collect();
    let mut cmd = CommandBuilder::new(parts[0]);
    if parts.len() > 1 { cmd.args(&parts[1..]); }
    cmd.env("TERM", "xterm-256color");

    // Set working directory + generate instructions
    let effective_wd = if let Some(ref wd) = working_dir {
        std::fs::create_dir_all(wd).ok();
        cmd.cwd(wd);
        eprintln!("[{name}] working dir: {}", wd.display());
        wd.clone()
    } else {
        let cwd = std::env::current_dir().unwrap_or_default();
        cmd.cwd(&cwd);
        cwd
    };

    // Generate backend-specific instruction files in working directory
    instructions::generate(&effective_wd, &command, &name);

    let _child = pair.slave.spawn_command(cmd)
        .unwrap_or_else(|e| panic!("[{name}] failed to spawn '{command}': {e}"));
    drop(pair.slave);

    let pty_writer: PtyWriter = Arc::new(Mutex::new(pair.master.take_writer().expect("take_writer")));
    let mut pty_reader = pair.master.try_clone_reader().expect("clone_reader");
    let pty_master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>> = Arc::new(Mutex::new(pair.master));

    let core = Arc::new(Mutex::new(AgentCore {
        vterm: vterm::VTerm::new(cols, rows),
        subscribers: Vec::new(),
    }));

    // Detect inject behavior from backend
    let preset = backend::Backend::from_command(&command).map(|b| b.preset());
    let submit_key = preset.as_ref().map(|p| p.submit_key.to_owned()).unwrap_or_else(|| "\r".to_owned());
    let inject_prefix = preset.as_ref().map(|p| p.inject_prefix.to_owned()).unwrap_or_default();
    let typed_inject = preset.as_ref().map(|p| p.typed_inject).unwrap_or(false);

    // Register in global registry + writers map
    registry.lock().unwrap_or_else(|e| e.into_inner()).insert(name.clone(), AgentHandle {
        pty_writer: Arc::clone(&pty_writer),
        core: Arc::clone(&core),
        submit_key,
        inject_prefix,
        typed_inject,
    });
    agent_writers.lock().unwrap_or_else(|e| e.into_inner())
        .insert(name.clone(), Arc::clone(&pty_writer));

    // PTY read thread — feeds VTerm + broadcasts + reaps on exit
    let core2 = Arc::clone(&core);
    let pw = Arc::clone(&pty_writer);
    let reg_reaper = Arc::clone(&registry);
    let aw_reaper = Arc::clone(&agent_writers);
    let cm_reaper = Arc::clone(&channel_mgr);
    let n = name.clone();
    std::thread::Builder::new()
        .name(format!("{n}_pty_read"))
        .spawn(move || {
            let mut buf = [0u8; 8192];
            let mut detect_buf = Vec::with_capacity(4096);
            let mut dialog_dismissed = false;
            loop {
                match pty_reader.read(&mut buf) {
                    Ok(0) => {
                        eprintln!("[{n}] PTY closed — reaping session");
                        reg_reaper.lock().unwrap_or_else(|e| e.into_inner()).remove(&n);
                        aw_reaper.lock().unwrap_or_else(|e| e.into_inner()).remove(&n);
                        cm_reaper.lock().unwrap_or_else(|e| e.into_inner()).on_agent_removed(&n);
                        let _ = std::fs::remove_dir_all(paths::agent_dir(&n));
                        break;
                    }
                    Ok(n_bytes) => {
                        let data = &buf[..n_bytes];

                        // Auto-dismiss trust dialog
                        if !dialog_dismissed {
                            detect_buf.extend_from_slice(data);
                            if detect_buf.len() > 8192 { let d = detect_buf.len() - 8192; detect_buf.drain(..d); }
                            let clean = strip_ansi(&String::from_utf8_lossy(&detect_buf));
                            if clean.contains("Yes, I trust") || clean.contains("Yes, proceed") {
                                eprintln!("[{n}] auto-dismissing trust dialog");
                                let _ = pw.lock().unwrap_or_else(|e| e.into_inner()).write_all(b"\x1b[A\x1b[A\r");
                                dialog_dismissed = true;
                                detect_buf.clear();
                            }
                        }

                        // Feed VTerm + broadcast (under same lock = atomic)
                        {
                            let mut core = core2.lock().unwrap_or_else(|e| e.into_inner());
                            core.vterm.process(data);
                            core.broadcast(data);
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .unwrap();

    // MCP server thread
    let mcp_sock = mcp_socket_path(&name);
    let _ = std::fs::remove_file(&mcp_sock);
    let reg2 = Arc::clone(&registry);
    let cm2 = Arc::clone(&channel_mgr);
    let n3 = name.clone();
    std::thread::Builder::new()
        .name(format!("{n3}_mcp"))
        .spawn(move || {
            let listener = match UnixListener::bind(&mcp_sock) {
                Ok(l) => l,
                Err(e) => { eprintln!("[{n3}] MCP bind error: {e}"); return; }
            };
            eprintln!("[{n3}] MCP server on {}", mcp_sock.display());
            for stream in listener.incoming().flatten() {
                let reg = Arc::clone(&reg2);
                let ib = Arc::clone(&inbox_store);
                let cm = Arc::clone(&cm2);
                let agent_name = n3.clone();
                std::thread::spawn(move || handle_mcp_session(stream, &agent_name, &reg, &ib, &cm));
            }
        })
        .unwrap();

    // TUI socket server (blocks this thread)
    let listener = UnixListener::bind(&sock)
        .unwrap_or_else(|e| panic!("[{name}] failed to bind {}: {e}", sock.display()));
    eprintln!("[{name}] TUI socket on {} (cmd: {command})", sock.display());

    // Notify channel adapters that agent is ready
    channel_mgr.lock().unwrap_or_else(|e| e.into_inner()).on_agent_created(&name);

    let reg3 = Arc::clone(&registry);
    for stream in listener.incoming() {
        let mut stream = match stream { Ok(s) => s, Err(_) => continue };
        eprintln!("[{name}] TUI client connected");

        // Atomic subscribe + screen dump (under core lock — no output gap)
        let rx = {
            let reg = reg3.lock().unwrap_or_else(|e| e.into_inner());
            let agent = reg.get(&name).unwrap();
            let mut core = agent.core.lock().unwrap_or_else(|e| e.into_inner());
            let dump = core.vterm.dump_screen();
            // Subscribe BEFORE releasing lock — no output lost
            let rx = core.subscribe();
            // Send screen dump to client
            if write_frame(&mut stream, &dump).is_err() { continue; }
            rx
        };

        // Output thread: forward broadcast to this client
        let mut write_stream = stream.try_clone().expect("clone");
        let n4 = name.clone();
        std::thread::Builder::new()
            .name(format!("{n4}_tui_out"))
            .spawn(move || {
                loop {
                    match rx.recv() {
                        Ok(data) => { if write_frame(&mut write_stream, &data).is_err() { break; } }
                        Err(_) => break,
                    }
                }
                eprintln!("[{n4}] TUI output thread ended");
            })
            .unwrap();

        // Input thread: forward client input to PTY, handle resize
        let read_stream = stream;
        let pty_w = Arc::clone(&pty_writer);
        let pty_m = Arc::clone(&pty_master);
        let core3 = Arc::clone(&core);
        let n5 = name.clone();
        std::thread::Builder::new()
            .name(format!("{n5}_tui_in"))
            .spawn(move || {
                let mut reader = read_stream;
                loop {
                    match read_tagged_frame(&mut reader) {
                        Ok((TAG_DATA, data)) => {
                            if pty_w.lock().unwrap_or_else(|e| e.into_inner()).write_all(&data).is_err() { break; }
                        }
                        Ok((TAG_RESIZE, data)) if data.len() == 4 => {
                            let cols = u16::from_be_bytes([data[0], data[1]]);
                            let rows = u16::from_be_bytes([data[2], data[3]]);
                            eprintln!("[{n5}] resize: {cols}x{rows}");
                            let _ = pty_m.lock().unwrap_or_else(|e| e.into_inner()).resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
                            if let Ok(mut c) = core3.lock() { c.vterm.resize(cols, rows); }
                        }
                        _ => break,
                    }
                }
                eprintln!("[{n5}] TUI client disconnected");
            })
            .unwrap();
    }
}

// ── MCP Server ──────────────────────────────────────────────────────────

fn handle_mcp_session(stream: UnixStream, agent_name: &str, registry: &AgentRegistry, inbox_store: &Arc<inbox::InboxStore>, channel_mgr: &Arc<Mutex<channel::ChannelManager>>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut writer = stream;

    loop {
        let mut headers = String::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 { return; }
            if line.trim().is_empty() { break; }
            headers.push_str(&line);
        }
        let content_length = headers.lines()
            .find_map(|l| l.strip_prefix("Content-Length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
            .unwrap_or(0);
        if content_length == 0 { continue; }

        let mut body = vec![0u8; content_length];
        if reader.read_exact(&mut body).is_err() { return; }

        let req: serde_json::Value = match serde_json::from_slice(&body) { Ok(v) => v, Err(_) => continue };
        let id = req.get("id").cloned();
        let method = req["method"].as_str().unwrap_or("");

        let result = match method {
            "initialize" => serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "agend", "version": "0.1.0" }
            }),
            "tools/list" => mcp_tools_list(),
            "tools/call" => {
                let tool = req["params"]["name"].as_str().unwrap_or("");
                let args = &req["params"]["arguments"];
                match tool {
                    "send_to_instance" => handle_send_to_instance(
                        agent_name, args["instance_name"].as_str().unwrap_or(""),
                        args["message"].as_str().unwrap_or(""), registry, inbox_store),
                    "broadcast" => {
                        let message = args["message"].as_str().unwrap_or("");
                        let names: Vec<String> = registry.lock().unwrap_or_else(|e| e.into_inner())
                            .keys().filter(|k| *k != agent_name).cloned().collect();
                        let mut sent = Vec::new();
                        for target in &names {
                            handle_send_to_instance(agent_name, target, message, registry, inbox_store);
                            sent.push(target.clone());
                        }
                        serde_json::json!({"content": [{"type": "text", "text":
                            format!("{{\"broadcast\":true,\"sent_to\":{}}}", serde_json::json!(sent))
                        }]})
                    }
                    "list_instances" => {
                        let names: Vec<String> = registry.lock().unwrap_or_else(|e| e.into_inner()).keys().cloned().collect();
                        serde_json::json!({"content": [{"type": "text", "text": serde_json::json!({"instances": names}).to_string()}]})
                    }
                    "inbox" => {
                        if let Some(id) = args["id"].as_u64() {
                            match inbox_store.get(agent_name, id) {
                                Some(msg) => serde_json::json!({"content": [{"type": "text", "text":
                                    format!("[from {}] {}", msg.sender, msg.text)
                                }]}),
                                None => serde_json::json!({"content": [{"type": "text", "text": "message not found"}], "isError": true})
                            }
                        } else {
                            let msgs = inbox_store.list(agent_name);
                            let list: Vec<serde_json::Value> = msgs.iter().map(|m| serde_json::json!({
                                "id": m.id, "sender": m.sender,
                                "preview": m.text.chars().take(100).collect::<String>()
                            })).collect();
                            serde_json::json!({"content": [{"type": "text", "text": serde_json::json!({"messages": list}).to_string()}]})
                        }
                    }
                    "reply" => {
                        let text = args["text"].as_str().unwrap_or("");
                        if text.is_empty() {
                            serde_json::json!({"content": [{"type": "text", "text": "text is required"}], "isError": true})
                        } else {
                            let formatted = format!("[{}] {}", agent_name, text);
                            channel_mgr.lock().unwrap_or_else(|e| e.into_inner()).send_to_agent(agent_name, &formatted);
                            eprintln!("[daemon] {agent_name} replied to telegram: {}", text.chars().take(80).collect::<String>());
                            serde_json::json!({"content": [{"type": "text", "text": "{\"replied\":true}"}]})
                        }
                    }
                    "request_information" => {
                        let target = args["target_instance"].as_str().unwrap_or("");
                        let question = args["question"].as_str().unwrap_or("");
                        let ctx = args["context"].as_str().unwrap_or("");
                        let msg = if ctx.is_empty() { format!("[query from {agent_name}] {question}") }
                        else { format!("[query from {agent_name}] {question}\n\nContext: {ctx}") };
                        handle_send_to_instance(agent_name, target, &msg, registry, inbox_store)
                    }
                    "delegate_task" => {
                        let target = args["target_instance"].as_str().unwrap_or("");
                        let task = args["task"].as_str().unwrap_or("");
                        let criteria = args["success_criteria"].as_str().unwrap_or("");
                        let ctx = args["context"].as_str().unwrap_or("");
                        let mut msg = format!("[task from {agent_name}] {task}");
                        if !criteria.is_empty() { msg.push_str(&format!("\n\nSuccess criteria: {criteria}")); }
                        if !ctx.is_empty() { msg.push_str(&format!("\n\nContext: {ctx}")); }
                        handle_send_to_instance(agent_name, target, &msg, registry, inbox_store)
                    }
                    "report_result" => {
                        let target = args["target_instance"].as_str().unwrap_or("");
                        let summary = args["summary"].as_str().unwrap_or("");
                        let artifacts = args["artifacts"].as_str().unwrap_or("");
                        let mut msg = format!("[result from {agent_name}] {summary}");
                        if !artifacts.is_empty() { msg.push_str(&format!("\n\nArtifacts: {artifacts}")); }
                        handle_send_to_instance(agent_name, target, &msg, registry, inbox_store)
                    }
                    "describe_instance" => {
                        let name = args["name"].as_str().unwrap_or("");
                        let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
                        if reg.contains_key(name) {
                            serde_json::json!({"content": [{"type": "text", "text":
                                serde_json::json!({"name": name, "status": "running", "submit_key": reg[name].submit_key}).to_string()
                            }]})
                        } else {
                            serde_json::json!({"content": [{"type": "text", "text": format!("instance '{name}' not found")}], "isError": true})
                        }
                    }
                    "create_instance" => {
                        // TODO: dynamic instance creation (needs spawn_agent refactor)
                        serde_json::json!({"content": [{"type": "text", "text": "create_instance not yet implemented"}], "isError": true})
                    }
                    "delete_instance" => {
                        let name = args["name"].as_str().unwrap_or("");
                        let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(agent) = reg.get(name) {
                            // Send Ctrl+C + Ctrl+D to kill
                            let _ = agent.pty_writer.lock().unwrap_or_else(|e| e.into_inner()).write_all(b"\x03\x04");
                            eprintln!("[daemon] delete_instance: killed {name}");
                            serde_json::json!({"content": [{"type": "text", "text": format!("{{\"deleted\":\"{name}\"}}")}]})
                        } else {
                            serde_json::json!({"content": [{"type": "text", "text": format!("instance '{name}' not found")}], "isError": true})
                        }
                    }
                    _ => serde_json::json!({"content": [{"type": "text", "text": format!("unknown tool: {tool}")}], "isError": true})
                }
            }
            "notifications/initialized" | "notifications/cancelled" => continue,
            _ => continue,
        };

        if let Some(id) = id {
            let resp = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
            let body = resp.to_string();
            let msg = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
            if writer.write_all(msg.as_bytes()).is_err() { return; }
            let _ = writer.flush();
        }
    }
}

fn handle_send_to_instance(sender: &str, target: &str, message: &str, registry: &AgentRegistry, inbox_store: &Arc<inbox::InboxStore>) -> serde_json::Value {
    let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(agent) = reg.get(target) {
        let text = match inbox_store.store_or_inject(target, sender, message, &agent.submit_key) {
            inbox::InjectAction::Direct(t) | inbox::InjectAction::Notification(t) => t,
        };
        match inject_to_agent(agent, text.trim_end().as_bytes()) {
            Ok(_) => {
                eprintln!("[daemon] {sender} → {target}: {}", message.chars().take(80).collect::<String>());
                serde_json::json!({"content": [{"type": "text", "text": format!("{{\"sent\":true,\"target\":\"{target}\"}}")}]})
            }
            Err(e) => serde_json::json!({"content": [{"type": "text", "text": format!("write error: {e}")}], "isError": true})
        }
    } else {
        let available: Vec<String> = reg.keys().cloned().collect();
        serde_json::json!({"content": [{"type": "text", "text": format!("'{target}' not found. available: {available:?}")}], "isError": true})
    }
}

/// Inject text into an agent's PTY with proper per-backend behavior.
fn inject_to_agent(agent: &AgentHandle, text: &[u8]) -> std::io::Result<()> {
    let prefix = agent.inject_prefix.as_bytes();
    let submit = agent.submit_key.as_bytes();
    let mut w = agent.pty_writer.lock().unwrap_or_else(|e| e.into_inner());

    if agent.typed_inject {
        // Per-byte typed write for bubbletea/ink TUIs
        for byte in prefix.iter().chain(text.iter()) {
            w.write_all(&[*byte])?;
            w.flush()?;
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    } else {
        if !prefix.is_empty() {
            w.write_all(prefix)?;
            w.flush()?;
        }
        w.write_all(text)?;
        w.flush()?;
    }

    // Delay before submit key
    std::thread::sleep(std::time::Duration::from_millis(20));
    w.write_all(submit)?;
    w.flush()?;
    Ok(())
}

fn mcp_tools_list() -> serde_json::Value {
    serde_json::json!({"tools": [
        // ── Communication ──
        {"name":"reply","description":"Reply to a Telegram user. Use when you receive [user:NAME via telegram].",
         "inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}},
        {"name":"send_to_instance","description":"Send a message to another agent instance.",
         "inputSchema":{"type":"object","properties":{
             "instance_name":{"type":"string","description":"Target agent name"},
             "message":{"type":"string","description":"Message to send"},
             "request_kind":{"type":"string","enum":["query","task","report","update"],"description":"Message intent"},
             "requires_reply":{"type":"boolean","description":"Whether you expect a response"},
             "correlation_id":{"type":"string","description":"Link to a previous message"}
         },"required":["instance_name","message"]}},
        {"name":"request_information","description":"Ask another agent a question (expects reply).",
         "inputSchema":{"type":"object","properties":{
             "target_instance":{"type":"string"},
             "question":{"type":"string"},
             "context":{"type":"string","description":"Optional context"}
         },"required":["target_instance","question"]}},
        {"name":"delegate_task","description":"Delegate a task to another agent (expects result report).",
         "inputSchema":{"type":"object","properties":{
             "target_instance":{"type":"string"},
             "task":{"type":"string","description":"Task description"},
             "success_criteria":{"type":"string"},
             "context":{"type":"string"}
         },"required":["target_instance","task"]}},
        {"name":"report_result","description":"Report results back to the agent that delegated a task.",
         "inputSchema":{"type":"object","properties":{
             "target_instance":{"type":"string"},
             "summary":{"type":"string"},
             "correlation_id":{"type":"string"},
             "artifacts":{"type":"string","description":"File paths, commit hashes, URLs"}
         },"required":["target_instance","summary"]}},
        {"name":"broadcast","description":"Send a message to ALL other agents.",
         "inputSchema":{"type":"object","properties":{"message":{"type":"string"}},"required":["message"]}},
        // ── Fleet management ──
        {"name":"list_instances","description":"List all running agent instances.",
         "inputSchema":{"type":"object","properties":{}}},
        {"name":"describe_instance","description":"Get details about a specific instance.",
         "inputSchema":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}},
        {"name":"create_instance","description":"Create and start a new agent instance.",
         "inputSchema":{"type":"object","properties":{
             "name":{"type":"string","description":"Instance name"},
             "command":{"type":"string","description":"Command to run (e.g. 'claude --dangerously-skip-permissions')"},
             "working_directory":{"type":"string","description":"Working directory path"}
         },"required":["name","command"]}},
        {"name":"delete_instance","description":"Stop and remove an agent instance.",
         "inputSchema":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}},
        // ── Inbox ──
        {"name":"inbox","description":"Read inbox messages (long messages stored here).",
         "inputSchema":{"type":"object","properties":{"id":{"type":"integer","description":"Message ID (omit to list all)"}}}}
    ]})
}

// ── Main ────────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Parse --config flag
    let mut config_path: Option<std::path::PathBuf> = None;
    let mut filtered_args: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--config" || args[i] == "-c" {
            if let Some(p) = args.get(i + 1) {
                config_path = Some(std::path::PathBuf::from(p));
                i += 2;
                continue;
            }
        }
        filtered_args.push(args[i].clone());
        i += 1;
    }
    let args = filtered_args;

    if args.first().map(|s| s.as_str()) == Some("--shutdown") {
        // Find active daemon's ctrl socket
        if let Some(run) = paths::find_active_run_dir() {
            let ctrl = run.join("ctrl.sock");
            match UnixStream::connect(&ctrl) {
                Ok(mut s) => { let _ = s.write_all(b"shutdown"); eprintln!("[daemon] shutdown signal sent"); }
                Err(e) => eprintln!("[daemon] cannot connect to {}: {e}", ctrl.display()),
            }
        } else {
            eprintln!("[daemon] no active daemon found");
        }
        return;
    }

    // Initialize run directory
    paths::init();
    eprintln!("[daemon] run dir: {}", paths::run_dir().display());

    // Parse agents from CLI args or fleet.yaml
    let load_config = || -> Result<config::FleetConfig, String> {
        if let Some(ref p) = config_path {
            config::FleetConfig::load(p)
        } else {
            config::FleetConfig::find_and_load()
        }
    };

    let agents: Vec<(String, String, Option<std::path::PathBuf>)> = if !args.is_empty() {
        args.iter().map(|a| {
            if let Some((name, cmd)) = a.split_once(':') { (name.to_owned(), cmd.to_owned(), None) }
            else { (a.to_owned(), a.to_owned(), None) }
        }).collect()
    } else if let Ok(cfg) = load_config() {
        cfg.instances.iter().map(|(name, ic)| {
            let cmd = ic.build_command(&cfg.defaults);
            let wd = ic.working_dir_or(&cfg.defaults).map(|p| p.to_path_buf());
            (name.clone(), cmd, wd)
        }).collect()
    } else {
        vec![("shell".into(), "bash".into(), None)]
    };

    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let agent_writers: api::AgentWriters = Arc::new(Mutex::new(HashMap::new()));
    let inbox_store = inbox::InboxStore::new();
    let channel_mgr = channel::ChannelManager::new();
    eprintln!("[daemon] starting {} agent(s)", agents.len());

    for (name, command, wd) in &agents {
        eprintln!("[daemon]   {name}: {command}{}", wd.as_ref().map(|p| format!(" (cwd: {})", p.display())).unwrap_or_default());
    }

    // Setup channel adapters BEFORE spawning agents (so on_agent_created works)
    if let Ok(cfg) = load_config() {
        if let Some((token, group_id)) = cfg.telegram_config() {
            let adapter = telegram::TelegramAdapter::new(telegram::TelegramConfig { bot_token: token, group_id });
            channel_mgr.lock().unwrap_or_else(|e| e.into_inner())
                .add_adapter(Box::new(adapter));
        }
    }

    for (name, command, wd) in agents {
        std::fs::create_dir_all(paths::agent_dir(&name)).ok();
        let reg = Arc::clone(&registry);
        let aw = Arc::clone(&agent_writers);
        let ib = Arc::clone(&inbox_store);
        let cm = Arc::clone(&channel_mgr);
        std::thread::Builder::new()
            .name(format!("agent_{name}"))
            .spawn(move || {
                spawn_agent(name, command, wd, reg, aw, ib, cm);
            })
            .unwrap();
    }

    // Wait for agents to register before starting services
    std::thread::sleep(std::time::Duration::from_secs(1));

    // Start API socket
    api::start(Arc::clone(&agent_writers));

    // Channel poll thread — routes incoming messages to agents
    {
        let cm = Arc::clone(&channel_mgr);
        let aw = Arc::clone(&agent_writers);
        std::thread::Builder::new()
            .name("channel_poll".into())
            .spawn(move || {
                loop {
                    let msgs = cm.lock().unwrap_or_else(|e| e.into_inner()).poll_all();
                    for msg in msgs {
                        let w = aw.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(pw) = w.get(&msg.agent_target) {
                            let formatted = format!("[user:{} via telegram] {}\r", msg.sender, msg.text);
                            let _ = pw.lock().unwrap_or_else(|e| e.into_inner()).write_all(formatted.as_bytes());
                            eprintln!("[channel] {} → {}: {}", msg.sender, msg.agent_target,
                                msg.text.chars().take(60).collect::<String>());
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            })
            .unwrap();
    }

    // Graceful shutdown on Ctrl+C
    let ctrl_sock = paths::ctrl_socket();
    let ctrl_sock2 = ctrl_sock.clone();
    ctrlc::set_handler(move || {
        eprintln!("\n[daemon] shutting down...");
        if let Ok(mut s) = UnixStream::connect(&ctrl_sock2) {
            let _ = s.write_all(b"shutdown");
        }
    }).ok();

    // Control socket for shutdown
    let _ = std::fs::remove_file(&ctrl_sock);
    if let Ok(listener) = UnixListener::bind(&ctrl_sock) {
        eprintln!("[daemon] use `agend-daemon --shutdown` or Ctrl+C to stop");
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 64];
            let _ = stream.read(&mut buf);
        }
    }

    eprintln!("[daemon] cleaning up...");
    paths::cleanup();
    std::process::exit(0);
}

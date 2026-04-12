#![allow(dead_code, unused_imports)]
//! agend-daemon: multi-agent PTY manager.

use agend_pty_poc::{
    api, backend, channel, config, event_log, features, fleet_store, git, health, inbox,
    instructions, mcp_config, paths, scheduler, state, telegram, vterm,
};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};

const TAG_DATA: u8 = 0;
const TAG_RESIZE: u8 = 1;
const MAX_FRAME_SIZE: usize = 1_000_000;
const PTY_BUF_SIZE: usize = 8192;
const DETECT_BUF_CAP: usize = 4096;
const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 40;
const DEPENDENCY_READY_TIMEOUT_SECS: u64 = 60;

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
    if len > MAX_FRAME_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok((tag[0], buf))
}

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
    state_machine: Arc<Mutex<state::StateMachine>>,
    health: Arc<Mutex<health::HealthMonitor>>,
}

type AgentRegistry = Arc<Mutex<HashMap<String, AgentHandle>>>;
type AgentTickInfo = (
    String,
    Arc<Mutex<state::StateMachine>>,
    Arc<Mutex<health::HealthMonitor>>,
);

/// Spawn config for respawning crashed agents.
/// Holds persistent health/state monitors that survive respawn.
#[derive(Clone)]
struct SpawnConfig {
    name: String,
    command: String,
    working_dir: Option<std::path::PathBuf>,
    worktree: bool,
    branch_name: Option<String>,
    state_machine: Arc<Mutex<state::StateMachine>>,
    health: Arc<Mutex<health::HealthMonitor>>,
}

type SpawnConfigs = Arc<Mutex<HashMap<String, SpawnConfig>>>;

/// Handle a HealthAction — called from both PTY read loop and tick thread.
#[allow(clippy::too_many_arguments)]
fn handle_health_action(
    action: &health::HealthAction,
    name: &str,
    registry: &AgentRegistry,
    agent_writers: &api::AgentWriters,
    agent_states: &api::AgentStateMap,
    spawn_configs: &SpawnConfigs,
    inbox_store: &Arc<inbox::InboxStore>,
    channel_mgr: &Arc<Mutex<channel::ChannelManager>>,
) {
    match action {
        health::HealthAction::Restart => {
            tracing::warn!(agent = %name, "scheduling respawn");
            do_respawn(
                name,
                registry,
                agent_writers,
                agent_states,
                spawn_configs,
                inbox_store,
                channel_mgr,
            );
        }
        health::HealthAction::KillAndRestart => {
            tracing::warn!(agent = %name, "hang detected — killing and respawning");
            // Send Ctrl+C + EOF to kill the process
            if let Some(pw) = agent_writers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(name)
            {
                let _ = pw
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .write_all(b"\x03\x04");
            }
            // Respawn will happen on next tick after process exits
        }
        health::HealthAction::MarkFailed => {
            tracing::warn!(agent = %name, "marked FAILED — no more restarts");
        }
        health::HealthAction::None => {}
    }
}

fn do_respawn(
    name: &str,
    registry: &AgentRegistry,
    agent_writers: &api::AgentWriters,
    agent_states: &api::AgentStateMap,
    spawn_configs: &SpawnConfigs,
    inbox_store: &Arc<inbox::InboxStore>,
    channel_mgr: &Arc<Mutex<channel::ChannelManager>>,
) {
    let cfg = match spawn_configs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(name)
        .cloned()
    {
        Some(c) => c,
        None => {
            tracing::warn!(agent = %name, "no spawn config for respawn");
            return;
        }
    };

    let now = std::time::Instant::now();
    if let Ok(mut h) = cfg.health.lock() {
        h.on_restart(now);
    }
    if let Ok(mut s) = cfg.state_machine.lock() {
        s.on_restart(now);
        s.on_restart_complete(now);
    }

    let reg = Arc::clone(registry);
    let aw = Arc::clone(agent_writers);
    let as_ = Arc::clone(agent_states);
    let ib = Arc::clone(inbox_store);
    let cm = Arc::clone(channel_mgr);
    let sc = Arc::clone(spawn_configs);
    std::thread::Builder::new()
        .name(format!("respawn_{}", name))
        .spawn(move || {
            tracing::warn!(agent = %cfg.name, "respawning");
            spawn_agent(
                cfg.name,
                cfg.command,
                cfg.working_dir,
                cfg.worktree,
                cfg.branch_name,
                reg,
                aw,
                as_,
                ib,
                cm,
                sc,
            );
        })
        .ok();
}

fn socket_path(name: &str) -> std::path::PathBuf {
    paths::tui_socket(name)
}

/// Unified PTY write — appends submit_key and writes atomically.
fn inject_to_pty(writer: &PtyWriter, text: &str, submit_key: &str) {
    let msg = format!("{text}{submit_key}");
    let _ = writer
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .write_all(msg.as_bytes());
}

fn setup_mcp_config(name: &str) -> (std::path::PathBuf, String) {
    let mcp_bin = paths::exe_sibling("agend-mcp");
    let mcp_config_path = paths::agent_dir(name).join("mcp-config.json");
    let mcp_config = serde_json::json!({
        "mcpServers": { format!("agend-{name}"): {
            "command": mcp_bin.display().to_string(),
            "args": [],
            "env": { "AGEND_INSTANCE_NAME": name }
        } }
    });
    if let Ok(json) = serde_json::to_string_pretty(&mcp_config) {
        std::fs::write(&mcp_config_path, json).ok();
    }
    let path_str = mcp_config_path.display().to_string();
    (mcp_config_path, path_str)
}

fn setup_prompt(name: &str, registry: &AgentRegistry) -> (std::path::PathBuf, String) {
    let prompt_path = paths::agent_dir(name).join("prompt.md");
    let others: Vec<String> = registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .filter(|k| k.as_str() != name)
        .cloned()
        .collect();
    std::fs::write(
        &prompt_path,
        format!(
            "You are '{}', part of an AI agent fleet. Other agents: {}.\n\
         Use `send_to_instance`/`list_instances` MCP tools. Respond directly to [message from X].",
            name,
            if others.is_empty() {
                "(none yet)".into()
            } else {
                others.join(", ")
            }
        ),
    )
    .ok();
    let path_str = prompt_path.display().to_string();
    (prompt_path, path_str)
}

fn inject_mcp_for_backend(
    command: &str,
    mcp_inject_flag: &str,
    mcp_config_path: &str,
    prompt_path: &str,
) -> String {
    if mcp_inject_flag.is_empty() {
        return command.to_owned();
    }
    if mcp_inject_flag == "--mcp-config" {
        format!(
            "{command} --mcp-config {mcp_config_path} --append-system-prompt-file {prompt_path}"
        )
    } else {
        format!("{command} {mcp_inject_flag} {mcp_config_path}")
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_agent(
    name: String,
    command: String,
    working_dir: Option<std::path::PathBuf>,
    worktree: bool,
    branch_name: Option<String>,
    registry: AgentRegistry,
    agent_writers: api::AgentWriters,
    agent_states: api::AgentStateMap,
    inbox_store: Arc<inbox::InboxStore>,
    channel_mgr: Arc<Mutex<channel::ChannelManager>>,
    spawn_configs: SpawnConfigs,
) {
    let sock = socket_path(&name);
    let _ = std::fs::remove_file(&sock);

    let (cols, rows) = crossterm::terminal::size().unwrap_or((DEFAULT_COLS, DEFAULT_ROWS));

    let pty_system = native_pty_system();
    let pair = match pty_system.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(agent = %name, error = %e, "failed to open PTY");
            return;
        }
    };

    let (_, mcp_config_path_str) = setup_mcp_config(&name);
    let preset = backend::Backend::from_command(&command).map(|b| b.preset());
    let (_, prompt_path_str) = setup_prompt(&name, &registry);

    let final_command = inject_mcp_for_backend(
        &command,
        preset.as_ref().map(|p| p.mcp_inject_flag).unwrap_or(""),
        &mcp_config_path_str,
        &prompt_path_str,
    );
    let final_command = if command.starts_with("gemini") && !final_command.contains("--resume") {
        format!("{final_command} --resume latest")
    } else {
        final_command
    };

    let parts: Vec<&str> = final_command.split_whitespace().collect();
    let mut cmd = CommandBuilder::new(parts[0]);
    if parts.len() > 1 {
        cmd.args(&parts[1..]);
    }
    cmd.env("TERM", "xterm-256color");

    let effective_wd = if let Some(ref wd) = working_dir {
        std::fs::create_dir_all(wd).ok();
        // Git worktree: redirect to isolated worktree directory
        let actual_wd = if worktree && git::is_git_repo(wd) {
            let custom_branch = branch_name.as_deref();
            match git::create_worktree(wd, &name, custom_branch) {
                Ok(wt) => {
                    tracing::info!(agent = %name, path = %wt.display(), "git worktree");
                    wt
                }
                Err(e) => {
                    tracing::error!(agent = %name, error = %e, "git worktree failed, using original dir");
                    wd.clone()
                }
            }
        } else {
            wd.clone()
        };
        cmd.cwd(&actual_wd);
        actual_wd
    } else {
        let cwd = std::env::current_dir().unwrap_or_default();
        cmd.cwd(&cwd);
        cwd
    };
    instructions::generate(&effective_wd, &command, &name);

    let _child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(agent = %name, command = %command, error = %e, "failed to spawn");
            return;
        }
    };
    drop(pair.slave);

    let pty_writer: PtyWriter = Arc::new(Mutex::new(match pair.master.take_writer() {
        Ok(w) => w,
        Err(e) => {
            tracing::error!(agent = %name, error = %e, "take_writer failed");
            return;
        }
    }));
    let mut pty_reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(agent = %name, error = %e, "clone_reader failed");
            return;
        }
    };
    let pty_master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>> =
        Arc::new(Mutex::new(pair.master));

    let core = Arc::new(Mutex::new(AgentCore {
        vterm: vterm::VTerm::new(cols, rows),
        subscribers: Vec::new(),
    }));

    let submit_key = preset
        .as_ref()
        .map(|p| p.submit_key.to_owned())
        .unwrap_or_else(|| "\r".to_owned());
    let inject_prefix = preset
        .as_ref()
        .map(|p| p.inject_prefix.to_owned())
        .unwrap_or_default();
    let typed_inject = preset.as_ref().map(|p| p.typed_inject).unwrap_or(false);

    let ready_pattern = preset.as_ref().map(|p| p.ready_pattern).unwrap_or(">");
    let (state_machine, health_monitor) = {
        let configs = spawn_configs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = configs.get(&name) {
            (
                Arc::clone(&existing.state_machine),
                Arc::clone(&existing.health),
            )
        } else {
            let state_patterns = state::StatePatterns::from_backend(ready_pattern);
            (
                Arc::new(Mutex::new(state::StateMachine::new(state_patterns))),
                Arc::new(Mutex::new(health::HealthMonitor::new())),
            )
        }
    };

    spawn_configs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            name.clone(),
            SpawnConfig {
                name: name.clone(),
                command: command.clone(),
                working_dir: working_dir.clone(),
                worktree,
                branch_name: branch_name.clone(),
                state_machine: Arc::clone(&state_machine),
                health: Arc::clone(&health_monitor),
            },
        );

    registry.lock().unwrap_or_else(|e| e.into_inner()).insert(
        name.clone(),
        AgentHandle {
            pty_writer: Arc::clone(&pty_writer),
            core: Arc::clone(&core),
            submit_key,
            inject_prefix,
            typed_inject,
            state_machine: Arc::clone(&state_machine),
            health: Arc::clone(&health_monitor),
        },
    );
    agent_writers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(name.clone(), Arc::clone(&pty_writer));
    agent_states
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            name.clone(),
            api::AgentStateHandle {
                state_machine: Arc::clone(&state_machine),
                health: Arc::clone(&health_monitor),
                working_dir: working_dir.clone(),
                role: None, // Set from fleet config after spawn
            },
        );

    // PTY read thread — feeds VTerm + broadcasts + reaps on exit
    let core2 = Arc::clone(&core);
    let pw = Arc::clone(&pty_writer);
    let reg_reaper = Arc::clone(&registry);
    let aw_reaper = Arc::clone(&agent_writers);
    let cm_reaper = Arc::clone(&channel_mgr);
    let sm = Arc::clone(&state_machine);
    let hm = Arc::clone(&health_monitor);
    let ib_reaper = Arc::clone(&inbox_store);
    let sc_reaper = Arc::clone(&spawn_configs);
    let as_reaper = Arc::clone(&agent_states);
    let dismiss_patterns: Vec<(String, Vec<u8>)> = preset
        .as_ref()
        .map(|p| {
            p.dismiss_patterns
                .iter()
                .map(|(s, k)| (s.to_string(), k.to_vec()))
                .collect()
        })
        .unwrap_or_default();
    let n = name.clone();
    std::thread::Builder::new()
        .name(format!("{n}_pty_read"))
        .spawn(move || {
            let mut buf = [0u8; PTY_BUF_SIZE];
            let mut detect_buf = Vec::with_capacity(DETECT_BUF_CAP);
            let mut dismiss_count = 0u32;
            loop {
                match pty_reader.read(&mut buf) {
                    Ok(0) => {
                        tracing::warn!(agent = %n, "PTY closed — reaping session");
                        event_log::log_event("pty_closed", &n, "");
                        // 1. Update state machine, record health action (but don't execute yet)
                        let now = std::time::Instant::now();
                        let action = if let Ok(mut s) = sm.lock() {
                            if let Some(new_state) = s.on_exit(now) {
                                tracing::info!(agent = %n, state = ?new_state, "state changed");
                                event_log::log_event("state_change", &n, &format!("{new_state:?}"));
                                if let Ok(mut h) = hm.lock() {
                                    let a = h.on_state_change(
                                        new_state,
                                        s.consecutive_errors(),
                                        s.last_error_kind(),
                                        now,
                                    );
                                    tracing::warn!(agent = %n, action = ?a, "health action");
                                    a
                                } else {
                                    health::HealthAction::None
                                }
                            } else {
                                health::HealthAction::None
                            }
                        } else {
                            health::HealthAction::None
                        };

                        // 2. Cleanup first — remove from registry, writers, notify channels
                        reg_reaper
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&n);
                        aw_reaper
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&n);
                        cm_reaper
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .on_agent_removed(&n);
                        let _ = std::fs::remove_dir_all(paths::agent_dir(&n));

                        // 3. Now safe to respawn (cleanup complete, no race)
                        handle_health_action(
                            &action,
                            &n,
                            &reg_reaper,
                            &aw_reaper,
                            &as_reaper,
                            &sc_reaper,
                            &ib_reaper,
                            &cm_reaper,
                        );
                        break;
                    }
                    Ok(n_bytes) => {
                        let data = &buf[..n_bytes];

                        // Auto-dismiss trust dialog
                        if dismiss_count < 5 && !dismiss_patterns.is_empty() {
                            detect_buf.extend_from_slice(data);
                            if detect_buf.len() > PTY_BUF_SIZE {
                                let d = detect_buf.len() - PTY_BUF_SIZE;
                                detect_buf.drain(..d);
                            }
                            let clean = state::strip_ansi(&String::from_utf8_lossy(&detect_buf));
                            for (pattern, key_seq) in &dismiss_patterns {
                                if clean.contains(pattern.as_str()) {
                                    tracing::debug!(agent = %n, pattern = %pattern, "auto-dismissing dialog");
                                    let _ = pw
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .write_all(key_seq);
                                    dismiss_count += 1;
                                    detect_buf.clear();
                                    break;
                                }
                            }
                        }

                        // Feed state machine with stripped output
                        {
                            let clean = state::strip_ansi(&String::from_utf8_lossy(data));
                            if let Ok(mut s) = sm.lock() {
                                if let Some(new_state) =
                                    s.process_output(&clean, std::time::Instant::now())
                                {
                                    tracing::info!(agent = %n, state = ?new_state, "state changed");
                                    event_log::log_event(
                                        "state_change",
                                        &n,
                                        &format!("{new_state:?}"),
                                    );
                                    if let Ok(mut h) = hm.lock() {
                                        let action = h.on_state_change(
                                            new_state,
                                            s.consecutive_errors(),
                                            s.last_error_kind(),
                                            std::time::Instant::now(),
                                        );
                                        if action != health::HealthAction::None {
                                            tracing::warn!(agent = %n, action = ?action, "health action");
                                            event_log::log_event(
                                                "health_action",
                                                &n,
                                                &format!("{action:?}"),
                                            );
                                            handle_health_action(
                                                &action,
                                                &n,
                                                &reg_reaper,
                                                &aw_reaper,
                                                &as_reaper,
                                                &sc_reaper,
                                                &ib_reaper,
                                                &cm_reaper,
                                            );
                                        }
                                    }
                                }
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
        .ok();

    // TUI socket server (blocks this thread)
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(agent = %name, path = %sock.display(), error = %e, "failed to bind TUI socket");
            return;
        }
    };
    tracing::info!(agent = %name, path = %sock.display(), command = %command, "TUI socket ready");

    channel_mgr
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .on_agent_created(&name);

    let reg3 = Arc::clone(&registry);
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        tracing::debug!(agent = %name, "TUI client connected");

        // Atomic subscribe + screen dump (under core lock — no output gap)
        let rx = {
            let reg = reg3.lock().unwrap_or_else(|e| e.into_inner());
            let agent = match reg.get(&name) {
                Some(a) => a,
                None => continue,
            };
            let mut core = agent.core.lock().unwrap_or_else(|e| e.into_inner());
            let dump = core.vterm.dump_screen();
            // Subscribe BEFORE releasing lock — no output lost
            let rx = core.subscribe();
            // Send screen dump to client
            if write_frame(&mut stream, &dump).is_err() {
                continue;
            }
            rx
        };

        // Output thread: forward broadcast to this client
        let mut write_stream = match stream.try_clone() {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(agent = %name, error = %e, "TUI clone failed");
                continue;
            }
        };
        let n4 = name.clone();
        std::thread::Builder::new()
            .name(format!("{n4}_tui_out"))
            .spawn(move || {
                while let Ok(data) = rx.recv() {
                    if write_frame(&mut write_stream, &data).is_err() {
                        break;
                    }
                }
                tracing::debug!(agent = %n4, "TUI output thread ended");
            })
            .ok();

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
                            if pty_w
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .write_all(&data)
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok((TAG_RESIZE, data)) if data.len() == 4 => {
                            let cols = u16::from_be_bytes([data[0], data[1]]);
                            let rows = u16::from_be_bytes([data[2], data[3]]);
                            tracing::debug!(agent = %n5, cols, rows, "resize");
                            let _ =
                                pty_m
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .resize(PtySize {
                                        rows,
                                        cols,
                                        pixel_width: 0,
                                        pixel_height: 0,
                                    });
                            if let Ok(mut c) = core3.lock() {
                                c.vterm.resize(cols, rows);
                            }
                        }
                        _ => break,
                    }
                }
                tracing::debug!(agent = %n5, "TUI client disconnected");
            })
            .ok();
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Parse --config and --dry-run flags
    let mut config_path: Option<std::path::PathBuf> = None;
    let mut dry_run = false;
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
        if args[i] == "--dry-run" {
            dry_run = true;
            i += 1;
            continue;
        }
        filtered_args.push(args[i].clone());
        i += 1;
    }
    let args = filtered_args;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("AGEND_LOG")
                .unwrap_or_else(|_| "info".parse().expect("valid filter")),
        )
        .with_target(false)
        .init();

    if dry_run {
        let cfg = if let Some(ref p) = config_path {
            config::FleetConfig::load(p).unwrap_or_else(|e| {
                eprintln!("{e}");
                std::process::exit(1);
            })
        } else {
            config::FleetConfig::find_and_load().unwrap_or_else(|e| {
                eprintln!("{e}");
                std::process::exit(1);
            })
        };
        features::dry_run(&cfg);
        return;
    }

    if args.first().map(|s| s.as_str()) == Some("--shutdown") {
        // Find active daemon's ctrl socket
        if let Some(run) = paths::find_active_run_dir() {
            let ctrl = run.join("ctrl.sock");
            match UnixStream::connect(&ctrl) {
                Ok(mut s) => {
                    let _ = s.write_all(b"shutdown");
                    tracing::info!("shutdown signal sent");
                }
                Err(e) => {
                    tracing::error!(path = %ctrl.display(), error = %e, "cannot connect to ctrl socket")
                }
            }
        } else {
            tracing::error!("no active daemon found");
        }
        return;
    }

    // Initialize run directory
    paths::init();
    tracing::info!(path = %paths::run_dir().display(), "run dir");

    // Acquire daemon lock (prevents duplicate fleet daemons)
    let fleet_id = config_path.as_ref().map(|p| p.display().to_string());
    let _lock_file = match paths::acquire_lock(fleet_id.as_deref()) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    tracing::info!("lock acquired");
    if !git::has_git() {
        tracing::warn!("git not found — worktree disabled. Install: brew install git");
    }

    // Parse agents from CLI args or fleet.yaml
    let load_config = || -> Result<config::FleetConfig, String> {
        if let Some(ref p) = config_path {
            config::FleetConfig::load(p)
        } else {
            config::FleetConfig::find_and_load()
        }
    };

    #[allow(clippy::type_complexity)]
    let agents: Vec<(
        String,
        String,
        Option<std::path::PathBuf>,
        bool,
        Option<String>,
    )> = if !args.is_empty() {
        args.iter()
            .map(|a| {
                if let Some((name, cmd)) = a.split_once(':') {
                    (name.to_owned(), cmd.to_owned(), None, false, None)
                } else {
                    (a.to_owned(), a.to_owned(), None, false, None)
                }
            })
            .collect()
    } else if let Ok(cfg) = load_config() {
        cfg.instances
            .iter()
            .map(|(name, ic)| {
                let cmd = ic.build_command(&cfg.defaults);
                let wd = Some(ic.effective_working_dir(&cfg.defaults, name));
                (
                    name.clone(),
                    cmd,
                    wd,
                    ic.worktree_enabled(&cfg.defaults),
                    ic.branch.clone(),
                )
            })
            .collect()
    } else {
        vec![("shell".into(), "bash".into(), None, false, None)]
    };

    // Merge dynamic instances from previous runs
    let mut agents = agents;
    for di in api::load_dynamic_instances() {
        if !agents.iter().any(|(n, _, _, _, _)| n == &di.name) {
            agents.push((
                di.name,
                di.command,
                di.working_dir.map(std::path::PathBuf::from),
                di.worktree,
                di.branch,
            ));
        }
    }

    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let agent_writers: api::AgentWriters = Arc::new(Mutex::new(HashMap::new()));
    let agent_states: api::AgentStateMap = Arc::new(Mutex::new(HashMap::new()));
    let spawn_configs: SpawnConfigs = Arc::new(Mutex::new(HashMap::new()));

    // Warn if multiple instances share working_directory
    {
        let mut seen: HashMap<String, Vec<String>> = HashMap::new();
        for (name, _, wd, _, _) in &agents {
            seen.entry(
                wd.as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            )
            .or_default()
            .push(name.clone());
        }
        for (dir, names) in &seen {
            if names.len() > 1 && !dir.is_empty() {
                tracing::warn!(agents = ?names, dir = %dir, "agents share working_directory");
            }
        }
    }
    let inbox_store = inbox::InboxStore::new();
    let channel_mgr = channel::ChannelManager::new();
    tracing::info!(count = agents.len(), "starting agents");

    for (name, command, wd, _, _) in &agents {
        let wd_str = wd
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        tracing::info!(agent = %name, command = %command, cwd = %wd_str, "agent configured");
    }

    // Setup channel adapters BEFORE spawning agents (so on_agent_created works)
    if let Ok(cfg) = load_config() {
        if let Some((token, group_id)) = cfg.telegram_config() {
            let adapter = telegram::TelegramAdapter::new(telegram::TelegramConfig {
                bot_token: token,
                group_id,
            });
            channel_mgr
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .add_adapter(Box::new(adapter));
        }
    }

    // Spawn agents with dependency ordering
    let dep_layers = if let Ok(cfg) = load_config() {
        features::dependency_layers(&cfg).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "dependency error, spawning all at once");
            vec![agents.iter().map(|(n, _, _, _, _)| n.clone()).collect()]
        })
    } else {
        vec![agents.iter().map(|(n, _, _, _, _)| n.clone()).collect()]
    };

    let agent_map: HashMap<String, (String, Option<std::path::PathBuf>, bool, Option<String>)> =
        agents
            .into_iter()
            .map(|(n, c, w, gw, gb)| (n, (c, w, gw, gb)))
            .collect();

    for (layer_idx, layer) in dep_layers.iter().enumerate() {
        if layer_idx > 0 {
            tracing::info!(
                layer = layer_idx - 1,
                "waiting for layer agents to be ready"
            );
            // Wait up to 60s for previous layer agents to reach Ready
            let deadline = std::time::Instant::now()
                + std::time::Duration::from_secs(DEPENDENCY_READY_TIMEOUT_SECS);
            'wait: loop {
                if std::time::Instant::now() > deadline {
                    tracing::warn!("timeout waiting for dependencies, proceeding");
                    break;
                }
                let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
                let all_ready = dep_layers[layer_idx - 1].iter().all(|name| {
                    reg.get(name)
                        .and_then(|h| h.state_machine.lock().ok())
                        .map(|s| {
                            matches!(
                                s.state(),
                                state::AgentState::Ready
                                    | state::AgentState::Busy
                                    | state::AgentState::Idle
                            )
                        })
                        .unwrap_or(false)
                });
                drop(reg);
                if all_ready {
                    break 'wait;
                }
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        }

        tracing::info!(layer = layer_idx, agents = ?layer, "spawning layer");
        for name in layer {
            let (command, wd, gw, gb) = match agent_map.get(name) {
                Some(v) => v.clone(),
                None => continue,
            };
            std::fs::create_dir_all(paths::agent_dir(name)).ok();
            let reg = Arc::clone(&registry);
            let aw = Arc::clone(&agent_writers);
            let as_ = Arc::clone(&agent_states);
            let ib = Arc::clone(&inbox_store);
            let cm = Arc::clone(&channel_mgr);
            let sc = Arc::clone(&spawn_configs);
            let n = name.clone();
            std::thread::Builder::new()
                .name(format!("agent_{n}"))
                .spawn(move || {
                    spawn_agent(n, command, wd, gw, gb, reg, aw, as_, ib, cm, sc);
                })
                .ok();
        }
        // Brief pause for agents to register
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    // Configure session timers and roles from fleet config
    if let Ok(cfg) = load_config() {
        let default_hours = cfg.defaults.max_session_hours;
        for (name, ic) in &cfg.instances {
            let hours = ic.max_session_hours.or(default_hours);
            if let Some(h) = hours {
                if let Some(sc) = spawn_configs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(name)
                {
                    if let Ok(mut hm) = sc.health.lock() {
                        hm.set_max_session_hours(h);
                    }
                }
            }
            // Set role on agent state handle
            if let Some(ref role) = ic.role {
                if let Some(handle) = agent_states
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_mut(name)
                {
                    handle.role = Some(role.clone());
                }
            }
        }
    }

    // Build API-visible spawn configs from daemon's spawn configs
    let api_spawn_configs: Arc<Mutex<HashMap<String, api::SpawnConfigInfo>>> = {
        let configs = spawn_configs.lock().unwrap_or_else(|e| e.into_inner());
        let map: HashMap<String, api::SpawnConfigInfo> = configs
            .iter()
            .map(|(name, sc)| {
                (
                    name.clone(),
                    api::SpawnConfigInfo {
                        name: name.clone(),
                        command: sc.command.clone(),
                        working_dir: sc.working_dir.clone(),
                        worktree: sc.worktree,
                        branch: sc.branch_name.clone(),
                    },
                )
            })
            .collect();
        Arc::new(Mutex::new(map))
    };

    // Spawn request channel (create_instance sends here, daemon thread spawns)
    let (spawn_tx, spawn_rx) = crossbeam::channel::unbounded::<api::SpawnConfigInfo>();

    // Start API socket
    api::start(Arc::new(api::DaemonCtx {
        writers: Arc::clone(&agent_writers),
        states: Arc::clone(&agent_states),
        spawn_configs: Arc::clone(&api_spawn_configs),
        inbox: Arc::clone(&inbox_store),
        channel_mgr: Arc::clone(&channel_mgr),
        spawn_tx,
    }));

    // Spawn request handler thread
    {
        let reg = Arc::clone(&registry);
        let aw = Arc::clone(&agent_writers);
        let as_ = Arc::clone(&agent_states);
        let ib = Arc::clone(&inbox_store);
        let cm = Arc::clone(&channel_mgr);
        let sc = Arc::clone(&spawn_configs);
        let asc = Arc::clone(&api_spawn_configs);
        std::thread::Builder::new()
            .name("spawn_handler".into())
            .spawn(move || {
                while let Ok(info) = spawn_rx.recv() {
                    let name = info.name.clone();
                    // Keep API spawn_configs in sync
                    asc.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(name.clone(), info.clone());
                    std::fs::create_dir_all(paths::agent_dir(&name)).ok();
                    let reg2 = Arc::clone(&reg);
                    let aw2 = Arc::clone(&aw);
                    let as2 = Arc::clone(&as_);
                    let ib2 = Arc::clone(&ib);
                    let cm2 = Arc::clone(&cm);
                    let sc2 = Arc::clone(&sc);
                    std::thread::Builder::new()
                        .name(format!("agent_{name}"))
                        .spawn(move || {
                            spawn_agent(
                                info.name,
                                info.command,
                                info.working_dir,
                                info.worktree,
                                info.branch,
                                reg2,
                                aw2,
                                as2,
                                ib2,
                                cm2,
                                sc2,
                            );
                        })
                        .ok();
                }
            })
            .ok();
    }

    // Channel poll thread — routes incoming messages to agents
    {
        let cm = Arc::clone(&channel_mgr);
        let aw = Arc::clone(&agent_writers);
        let reg_poll = Arc::clone(&registry);
        std::thread::Builder::new()
            .name("channel_poll".into())
            .spawn(move || {
                loop {
                    let msgs = cm.lock().unwrap_or_else(|e| e.into_inner()).poll_all();
                    for msg in msgs {
                        // Get submit_key from registry for this agent
                        let submit_key = reg_poll
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .get(&msg.agent_target)
                            .map(|h| h.submit_key.clone())
                            .unwrap_or_else(|| "\r".into());
                        let w = aw.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(pw) = w.get(&msg.agent_target) {
                            let formatted =
                                format!("[user:{} via telegram] {}", msg.sender, msg.text);
                            inject_to_pty(pw, &formatted, &submit_key);
                            tracing::debug!(
                                sender = %msg.sender,
                                target = %msg.agent_target,
                                preview = %msg.text.chars().take(60).collect::<String>(),
                                "channel message routed"
                            );
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            })
            .ok();
    }

    // Health tick thread — drives time-based state transitions + health actions
    {
        let reg = Arc::clone(&registry);
        let aw = Arc::clone(&agent_writers);
        let as2 = Arc::clone(&agent_states);
        let sc = Arc::clone(&spawn_configs);
        let ib = Arc::clone(&inbox_store);
        let cm = Arc::clone(&channel_mgr);
        std::thread::Builder::new()
            .name("health_tick".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(3));
                    let now = std::time::Instant::now();

                    // Snapshot agent names + their Arc handles to avoid holding registry lock
                    let agents: Vec<AgentTickInfo> = {
                        let reg = reg.lock().unwrap_or_else(|e| e.into_inner());
                        reg.iter()
                            .map(|(name, handle)| {
                                (
                                    name.clone(),
                                    Arc::clone(&handle.state_machine),
                                    Arc::clone(&handle.health),
                                )
                            })
                            .collect()
                    };

                    for (name, sm, hm) in &agents {
                        // Tick state machine (idle detection, error hysteresis confirmation)
                        if let Ok(mut s) = sm.lock() {
                            if let Some(new_state) = s.tick(now) {
                                tracing::debug!(agent = %name, state = ?new_state, "tick state changed");
                                event_log::log_event(
                                    "state_change",
                                    name,
                                    &format!("{new_state:?}"),
                                );
                                if let Ok(mut h) = hm.lock() {
                                    let action = h.on_state_change(
                                        new_state,
                                        s.consecutive_errors(),
                                        s.last_error_kind(),
                                        now,
                                    );
                                    if action != health::HealthAction::None {
                                        tracing::debug!(agent = %name, action = ?action, "tick health action");
                                        handle_health_action(
                                            &action, name, &reg, &aw, &as2, &sc, &ib, &cm,
                                        );
                                    }
                                }
                            }
                        }

                        // Tick health monitor (hang detection, backoff-gated restart, session timer)
                        if let (Ok(s), Ok(mut h)) = (sm.lock(), hm.lock()) {
                            if h.check_session_warning(now) {
                                tracing::warn!(agent = %name, "session nearing max duration (80%)");
                            }
                            let action = h.tick(s.state(), now);
                            if action != health::HealthAction::None {
                                tracing::debug!(agent = %name, action = ?action, "health tick action");
                                handle_health_action(&action, name, &reg, &aw, &as2, &sc, &ib, &cm);
                            }
                        }
                    }

                    // Check cron schedules
                    let epoch = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    for (id, target, message) in scheduler::check_due(epoch) {
                        let w = aw.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(pw) = w.get(&target) {
                            let formatted = format!("[scheduled] {message}");
                            inject_to_pty(pw, &formatted, "\r");
                            tracing::info!(schedule_id = %id, target = %target, "scheduled message sent");
                            scheduler::mark_run(&id);
                        }
                    }
                }
            })
            .ok();
    }

    // Graceful shutdown on Ctrl+C
    let ctrl_sock = paths::ctrl_socket();
    let ctrl_sock2 = ctrl_sock.clone();
    ctrlc::set_handler(move || {
        tracing::info!("shutting down...");
        if let Ok(mut s) = UnixStream::connect(&ctrl_sock2) {
            let _ = s.write_all(b"shutdown");
        }
    })
    .ok();

    // Control socket for shutdown
    let _ = std::fs::remove_file(&ctrl_sock);
    if let Ok(listener) = UnixListener::bind(&ctrl_sock) {
        tracing::info!("use `agend-daemon --shutdown` or Ctrl+C to stop");
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 64];
            let _ = stream.read(&mut buf);
        }
    }

    tracing::info!("cleaning up...");
    paths::cleanup();
    std::process::exit(0);
}

//! Agent CLI — clap subcommands for agent-to-agent communication.
//! Invoked as `agend-pty agent <command>`. Sends JSON-RPC to daemon API socket.

use clap::Subcommand;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};

use crate::ipc;

fn output(value: Value) {
    println!(
        "{}",
        serde_json::to_string(&value).unwrap_or_else(|_| "{}".into())
    );
    if value.get("error").is_some() {
        std::process::exit(1);
    }
}

/// Read message from positional arg or stdin.
fn get_text(text: Option<String>, stdin_flag: bool) -> String {
    if let Some(t) = text {
        if stdin_flag {
            read_stdin()
        } else {
            t
        }
    } else if stdin_flag || !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        read_stdin()
    } else {
        eprintln!("error: text argument required (or pipe via --stdin)");
        std::process::exit(1);
    }
}

fn read_stdin() -> String {
    let mut buf = String::new();
    if std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf).is_err() {
        output(json!({"error": "failed to read stdin"}));
    }
    buf.trim().to_owned()
}

fn api_call(method: &str, params: &Value) -> Value {
    let run = match crate::paths::find_active_run_dir() {
        Some(r) => r,
        None => {
            return json!({"error": "daemon not running"});
        }
    };
    match ipc::connect_named(&run, ipc::API_NAME) {
        Ok(mut s) => {
            s.set_read_timeout(Some(std::time::Duration::from_secs(30)))
                .ok();
            let req = json!({"method": method, "params": params});
            if writeln!(s, "{req}").is_err() {
                return json!({"error": "write failed"});
            }
            s.flush().ok();
            let mut line = String::new();
            if BufReader::new(s).read_line(&mut line).is_err() {
                return json!({"error": "read failed"});
            }
            serde_json::from_str::<Value>(line.trim())
                .unwrap_or_else(|_| json!({"error": "parse failed"}))
        }
        Err(e) => json!({"error": format!("connect: {e}")}),
    }
}

fn mcp_call(tool: &str, args: &Value) -> Value {
    let instance = std::env::var("AGEND_INSTANCE_NAME").unwrap_or_default();
    let r = api_call(
        "mcp_call",
        &json!({"instance": instance, "tool": tool, "arguments": args}),
    );
    // Unwrap MCP content wrapper if present
    if let Some(text) = r["result"]["content"][0]["text"].as_str() {
        serde_json::from_str(text).unwrap_or_else(|_| json!({"text": text}))
    } else if r.get("error").is_some() {
        r
    } else {
        r.get("result").cloned().unwrap_or(r)
    }
}

// ── Subcommands ─────────────────────────────────────────────────────────

#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// Send a message to another agent.
    Send {
        target: String,
        /// Message text (or use --stdin for piped input).
        message: Option<String>,
        /// Read message from stdin.
        #[arg(long)]
        stdin: bool,
    },
    /// Delegate a task to another agent.
    Delegate {
        target: String,
        /// Task description (or use --stdin).
        task: Option<String>,
        #[arg(long)]
        criteria: Option<String>,
        #[arg(long)]
        context: Option<String>,
        /// Read task from stdin.
        #[arg(long)]
        stdin: bool,
    },
    /// Report a result to another agent.
    Report {
        target: String,
        /// Summary (or use --stdin).
        summary: Option<String>,
        #[arg(long)]
        correlation_id: Option<String>,
        #[arg(long)]
        artifacts: Option<String>,
        /// Read summary from stdin.
        #[arg(long)]
        stdin: bool,
    },
    /// Ask another agent a question.
    Ask {
        target: String,
        /// Question (or use --stdin).
        question: Option<String>,
        #[arg(long)]
        context: Option<String>,
        /// Read question from stdin.
        #[arg(long)]
        stdin: bool,
    },
    /// Broadcast a message to all agents or a team.
    Broadcast {
        /// Message (or use --stdin).
        message: Option<String>,
        #[arg(long)]
        team: Option<String>,
        /// Read message from stdin.
        #[arg(long)]
        stdin: bool,
    },
    /// Drain the inbox.
    Inbox,
    /// Reply in the channel (Telegram).
    Reply {
        /// Reply text (or use --stdin).
        text: Option<String>,
        /// Read text from stdin.
        #[arg(long)]
        stdin: bool,
    },
    /// List running instances.
    #[command(alias = "ls")]
    List,
    /// Describe an instance.
    Describe { name: String },
    /// Spawn a new agent instance.
    Spawn {
        name: String,
        #[arg(long)]
        backend: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long, alias = "dir")]
        working_directory: Option<String>,
        #[arg(long)]
        branch: Option<String>,
    },
    /// Start a stopped instance.
    Start { name: String },
    /// Delete an instance.
    Delete { name: String },
    /// Replace an instance.
    Replace {
        name: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Task board operations.
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    /// Decision log operations.
    Decision {
        #[command(subcommand)]
        command: DecisionCommand,
    },
    /// Team management.
    Team {
        #[command(subcommand)]
        command: TeamCommand,
    },
    /// Schedule management.
    Schedule {
        #[command(subcommand)]
        command: ScheduleCommand,
    },
    /// React to a message.
    React { emoji: String, message_id: String },
    /// Edit a sent message.
    Edit { message_id: String, text: String },
}

#[derive(Debug, Subcommand)]
pub enum TaskCommand {
    /// Create a new task.
    Create {
        title: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        assignee: Option<String>,
    },
    /// List tasks.
    List,
    /// Claim a task.
    Claim { id: String },
    /// Mark a task as done.
    Done {
        id: String,
        #[arg(long)]
        result: Option<String>,
    },
    /// Update a task.
    Update {
        id: String,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        assignee: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum DecisionCommand {
    /// Post a new decision.
    Post { title: String, content: String },
    /// List decisions.
    List,
    /// Update a decision.
    Update {
        id: u64,
        #[arg(long)]
        content: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum TeamCommand {
    /// Create a team.
    Create { name: String, members: Vec<String> },
    /// List teams.
    List,
    /// Delete a team.
    Delete { name: String },
    /// Update team members.
    Update { name: String, members: Vec<String> },
}

#[derive(Debug, Subcommand)]
pub enum ScheduleCommand {
    /// Create a schedule.
    Create {
        cron: String,
        message: String,
        #[arg(long)]
        target: Option<String>,
    },
    /// List schedules.
    List,
    /// Delete a schedule.
    Delete { id: String },
    /// Update a schedule.
    Update {
        id: String,
        #[arg(long)]
        cron: Option<String>,
        #[arg(long)]
        message: Option<String>,
        #[arg(long)]
        enabled: Option<bool>,
    },
}

// ── Entry point ─────────────────────────────────────────────────────────

pub fn run(command: AgentCommand) {
    match command {
        AgentCommand::Send {
            target,
            message,
            stdin,
        } => {
            let msg = get_text(message, stdin);
            output(mcp_call(
                "send_to_instance",
                &json!({"instance_name": target, "message": msg}),
            ));
        }
        AgentCommand::Delegate {
            target,
            task,
            criteria,
            context,
            stdin,
        } => {
            let task = get_text(task, stdin);
            output(mcp_call(
                "delegate_task",
                &json!({"target_instance": target, "task": task, "success_criteria": criteria, "context": context}),
            ));
        }
        AgentCommand::Report {
            target,
            summary,
            correlation_id,
            artifacts,
            stdin,
        } => {
            let summary = get_text(summary, stdin);
            output(mcp_call(
                "report_result",
                &json!({"target_instance": target, "summary": summary, "correlation_id": correlation_id, "artifacts": artifacts}),
            ));
        }
        AgentCommand::Ask {
            target,
            question,
            context,
            stdin,
        } => {
            let question = get_text(question, stdin);
            output(mcp_call(
                "request_information",
                &json!({"target_instance": target, "question": question, "context": context}),
            ));
        }
        AgentCommand::Broadcast {
            message,
            team,
            stdin,
        } => {
            let msg = get_text(message, stdin);
            output(mcp_call(
                "broadcast",
                &json!({"message": msg, "team": team}),
            ));
        }
        AgentCommand::Inbox => {
            output(mcp_call("inbox", &json!({})));
        }
        AgentCommand::Reply { text, stdin } => {
            let text = get_text(text, stdin);
            output(mcp_call("reply", &json!({"text": text})));
        }
        AgentCommand::List => {
            output(mcp_call("list_instances", &json!({})));
        }
        AgentCommand::Describe { name } => {
            output(mcp_call(
                "describe_instance",
                &json!({"instance_name": name}),
            ));
        }
        AgentCommand::Spawn {
            name,
            backend,
            model,
            working_directory,
            branch,
        } => {
            output(mcp_call(
                "create_instance",
                &json!({"name": name, "backend": backend, "model": model, "working_directory": working_directory, "branch": branch}),
            ));
        }
        AgentCommand::Start { name } => {
            output(mcp_call("start_instance", &json!({"instance_name": name})));
        }
        AgentCommand::Delete { name } => {
            output(mcp_call("delete_instance", &json!({"instance_name": name})));
        }
        AgentCommand::Replace { name, reason } => {
            output(mcp_call(
                "replace_instance",
                &json!({"instance_name": name, "reason": reason}),
            ));
        }
        AgentCommand::Task { command } => match command {
            TaskCommand::Create {
                title,
                description,
                assignee,
            } => output(mcp_call(
                "task",
                &json!({"action": "create", "title": title, "description": description, "assignee": assignee}),
            )),
            TaskCommand::List => output(mcp_call("task", &json!({"action": "list"}))),
            TaskCommand::Claim { id } => {
                output(mcp_call("task", &json!({"action": "claim", "id": id})))
            }
            TaskCommand::Done { id, result } => output(mcp_call(
                "task",
                &json!({"action": "done", "id": id, "result": result}),
            )),
            TaskCommand::Update {
                id,
                status,
                assignee,
            } => output(mcp_call(
                "task",
                &json!({"action": "update", "id": id, "status": status, "assignee": assignee}),
            )),
        },
        AgentCommand::Decision { command } => match command {
            DecisionCommand::Post { title, content } => output(mcp_call(
                "decision",
                &json!({"action": "post", "title": title, "content": content}),
            )),
            DecisionCommand::List => output(mcp_call("decision", &json!({"action": "list"}))),
            DecisionCommand::Update { id, content } => output(mcp_call(
                "decision",
                &json!({"action": "update", "id": id, "content": content}),
            )),
        },
        AgentCommand::Team { command } => match command {
            TeamCommand::Create { name, members } => output(mcp_call(
                "team",
                &json!({"action": "create", "name": name, "members": members}),
            )),
            TeamCommand::List => output(mcp_call("team", &json!({"action": "list"}))),
            TeamCommand::Delete { name } => {
                output(mcp_call("team", &json!({"action": "delete", "name": name})))
            }
            TeamCommand::Update { name, members } => output(mcp_call(
                "team",
                &json!({"action": "update", "name": name, "members": members}),
            )),
        },
        AgentCommand::Schedule { command } => match command {
            ScheduleCommand::Create {
                cron,
                message,
                target,
            } => output(mcp_call(
                "schedule",
                &json!({"action": "create", "cron": cron, "message": message, "target": target}),
            )),
            ScheduleCommand::List => output(mcp_call("schedule", &json!({"action": "list"}))),
            ScheduleCommand::Delete { id } => {
                output(mcp_call("schedule", &json!({"action": "delete", "id": id})))
            }
            ScheduleCommand::Update {
                id,
                cron,
                message,
                enabled,
            } => output(mcp_call(
                "schedule",
                &json!({"action": "update", "id": id, "cron": cron, "message": message, "enabled": enabled}),
            )),
        },
        AgentCommand::React { emoji, message_id } => {
            output(mcp_call(
                "react",
                &json!({"emoji": emoji, "message_id": message_id}),
            ));
        }
        AgentCommand::Edit { message_id, text } => {
            output(mcp_call(
                "edit_message",
                &json!({"message_id": message_id, "text": text}),
            ));
        }
    }
}

# agend-pty

Persistent PTY-based fleet manager for AI coding agents. Run multiple agents (Claude, Kiro, Codex, Gemini) in managed pseudo-terminals with inter-agent messaging, git worktree isolation, and auto-recovery.

## 30-Second Start

```bash
cargo build --release
./target/release/agend-pty quickstart   # interactive setup wizard
./target/release/agend-pty daemon       # start the fleet
```

Or try the demo (no API key needed):

```bash
./target/release/agend-pty demo
```

## Install

```bash
# Build from source
git clone https://github.com/user/agend-pty.git
cd agend-pty
cargo build --release

# Binaries are in target/release/:
#   agend-pty      CLI entry point
#   agend-daemon   Persistent daemon
#   agend-tui      TUI client (Ctrl+B d to detach)
#   agend-mcp      MCP bridge (spawned automatically)
```

## Architecture

```
┌─────────────┐     ┌─────────────┐
│  agend-tui  │     │  agend-tui  │    TUI clients (attach/detach)
└──────┬──────┘     └──────┬──────┘
       │ Unix socket       │
┌──────┴───────────────────┴──────┐
│          agend-daemon           │    Daemon (persistent)
│  ┌─────────┐  ┌─────────┐      │
│  │ PTY: alice │ PTY: bob │ ...  │    One PTY per agent
│  └─────────┘  └─────────┘      │
│  ┌─────────────────────────┐    │
│  │  API socket (api.sock)  │    │    JSON-RPC + MCP protocol
│  └─────────────────────────┘    │
└─────────────────────────────────┘
```

See [docs/architecture.md](docs/architecture.md) for the full system design (state machine, health monitor, socket protocol, etc).

## CLI Commands

| Command | Description |
|---------|-------------|
| `quickstart` | Interactive setup wizard |
| `demo` | Run demo with mock agents (no API key) |
| `daemon` | Start the daemon |
| `attach <agent>` | Connect TUI to a running agent |
| `logs <agent> [-f]` | Stream agent output (read-only) |
| `status [--live]` | Show fleet status (--live for dashboard) |
| `list` | List agents in current fleet |
| `inject <agent> <msg>` | Send a message to an agent |
| `dry-run` | Validate fleet.yaml without starting |
| `snapshot [-o file]` | Save fleet state to JSON |
| `restore [-i file]` | Restore fleet from snapshot |
| `cleanup` | Remove leftover git worktrees |
| `bugreport` | Export diagnostic info to file |
| `doctor` | Check system health |
| `shutdown` | Stop a running daemon |

## Features

- **Multi-agent PTY management** via portable-pty (5 backends: Claude, Kiro, Codex, OpenCode, Gemini)
- **Daemon/client split** — agents persist across TUI disconnects
- **Git worktree isolation** — each agent gets its own branch, merge_preview/merge_all to integrate
- **8-state lifecycle machine** — Starting, Ready, Busy, Idle, Errored, Crashed, Restarting, WaitingForInput
- **Health monitor** — auto-respawn with exponential backoff, hang detection, session timer
- **Dependency ordering** — `depends_on` with Kahn's algorithm for layered startup
- **Cron scheduler** — schedule recurring messages to agents
- **Teams, Decisions, Task board** — fleet-wide coordination primitives
- **Telegram integration** — forum topics per agent, react, edit, reply
- **Live dashboard** — `status --live` with 2s polling ASCII display
- **Structured logging** — tracing with `AGEND_LOG` env filter
- **MCP socket pooling** — daemon handles MCP protocol natively
- **CI automation** — `watch_ci` checks GitHub PR status via gh CLI

### MCP Tools (23)

| Tool | Description |
|------|-------------|
| `reply` | Reply to a user (Telegram/Discord) |
| `send_to_instance` | Send a message to another agent |
| `request_information` | Ask another agent a question |
| `delegate_task` | Delegate a task to another agent |
| `report_result` | Report results back to an agent |
| `broadcast` | Send to all agents (or team members) |
| `list_instances` | List running agents |
| `describe_instance` | Get agent details |
| `delete_instance` | Stop an agent (optionally cleanup worktree) |
| `create_instance` | Create a new agent instance at runtime |
| `replace_instance` | Replace an agent with new settings (atomic swap) |
| `inbox` | Read inbox messages |
| `start_instance` | Restart a stopped/failed agent |
| `decision` | Decision operations (post/list/update) |
| `task` | Task board operations (create/list/claim/done/update) |
| `react` | React to a message with emoji |
| `edit_message` | Edit a sent message |
| `wait_for_idle` | Wait for an agent to become idle |
| `merge` | Git merge operations (preview/squash/all) |
| `team` | Team operations (create/list/delete/update) |
| `list_events` | List event log |
| `schedule` | Cron schedule operations (create/list/delete/update) |
| `watch_ci` | Check GitHub PR CI status via gh CLI |

## fleet.yaml Schema

```yaml
defaults:
  backend: claude            # claude, kiro, codex, opencode, gemini
  model: sonnet              # Optional default model
  working_directory: /path   # Optional default working dir
  worktree: true             # Git worktree isolation (default: true)
  max_session_hours: 8       # Optional session time limit

instances:
  agent-name:
    working_directory: /path
    backend: claude
    model: opus
    command: "custom cmd"      # Full command override
    skip_permissions: false
    depends_on: [other-agent]
    worktree: true
    branch: feature-branch
    max_session_hours: 4       # Per-instance override
    role: worker               # Tool filtering (worker/coordinator/reviewer)

channel:                       # Optional Telegram integration
  bot_token_env: TELEGRAM_BOT_TOKEN
  group_id: -100123456789
```

See [examples/](examples/) for ready-to-use configurations.

## Documentation

- [Architecture](docs/architecture.md) — system design, state machine, protocols
- [Changelog](CHANGELOG.md) — version history
- [Examples](examples/) — fleet.yaml templates

## License

[MIT](LICENSE)

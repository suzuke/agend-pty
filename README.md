# agend-pty

AI agent fleet orchestrator with PTY-based daemon architecture.

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
│  │  API socket (api.sock)  │    │    JSON-RPC over Unix socket
│  └─────────────────────────┘    │
│  ┌─────────────────────────┐    │
│  │  agend-mcp (per agent)  │    │    MCP server: stdin NDJSON ↔ API socket
│  └─────────────────────────┘    │
└─────────────────────────────────┘
```

## Quick Start

```bash
# Create fleet.yaml
cat > fleet.yaml << 'YAML'
defaults:
  backend: claude
  worktree: true

instances:
  alice:
    skip_permissions: true
    working_directory: /tmp/alice-workspace
    branch: feature-alice
  bob:
    skip_permissions: true
    working_directory: /tmp/bob-workspace
    depends_on: [alice]
YAML

# Start daemon
cargo run --bin agend-daemon

# Attach to an agent (in another terminal)
cargo run --bin agend-tui -- alice

# Detach: Ctrl+B d
# Shutdown: agend-pty shutdown
```

## CLI Usage

```bash
# From fleet.yaml (recommended)
agend-pty daemon

# From CLI args
agend-pty daemon alice:claude bob:bash

# Attach to agent
agend-pty attach alice

# Show running daemons
agend-pty status

# List agents in current fleet
agend-pty list

# Send a message to an agent
agend-pty inject alice "hello"

# Validate fleet.yaml without starting agents
agend-pty dry-run

# Save/restore fleet state
agend-pty snapshot -o fleet-snapshot.json
agend-pty restore -i fleet-snapshot.json

# Remove leftover git worktrees
agend-pty cleanup

# Health check
agend-pty doctor

# Shutdown
agend-pty shutdown
```

## Features

- **Multi-agent PTY management** via portable-pty
- **Daemon/client split** — agents persist across TUI disconnects
- **VTerm screen state** — reconnect gets proper screen dump (via alacritty_terminal)
- **Git worktree isolation** — each agent gets its own branch/worktree
- **Dependency ordering** — `depends_on` ensures agents start in correct order
- **Auto-dismiss trust dialogs** — Claude Code, Gemini, Codex trust prompts handled automatically
- **Terminal resize** — synced from TUI client to PTY
- **fleet.yaml config** — define agents, backends, working directories
- **Graceful shutdown** — Ctrl+C or `shutdown` command
- **Dry-run mode** — validate config without starting agents
- **Snapshot/restore** — save and restore fleet state
- **Health monitoring** — auto-respawn crashed agents with backoff
- **Cron scheduler** — schedule recurring messages to agents
- **Teams** — group agents for targeted broadcasts
- **Decisions** — fleet-wide shared decision log
- **Task board** — create, claim, and track tasks across agents
- **Event log** — append-only audit trail of state changes and health actions
- **Merge preview/merge** — preview and squash-merge agent worktree branches
- **Wait for idle** — block until an agent reaches idle/ready state

### MCP Tools (24)

The daemon exposes these tools to agents via the MCP protocol:

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
| `post_decision` | Post a fleet-wide decision |
| `list_decisions` | List fleet decisions |
| `update_decision` | Update a decision |
| `task` | Task board operations (create/list/claim/done/update) |
| `react` | React to a message with emoji |
| `edit_message` | Edit a sent message |
| `wait_for_idle` | Wait for an agent to become idle |
| `merge_preview` | Preview merge of agent branch |
| `merge_agent` | Squash merge agent branch |
| `team` | Team operations (create/list/delete/update) |
| `list_events` | List event log |
| `schedule` | Cron schedule operations (create/list/delete/update) |

## fleet.yaml Schema

```yaml
# Default settings applied to all instances
defaults:
  backend: claude          # Default backend: claude, kiro, codex, opencode, gemini
  model: sonnet            # Default model (optional, backend-specific)
  working_directory: /path # Default working directory
  worktree: true           # Enable git worktree isolation (default: true)

# Agent instance definitions
instances:
  agent-name:
    working_directory: /path  # Working directory (overrides default)
    backend: claude           # Backend override
    model: opus               # Model override
    command: "custom cmd"     # Full custom command (overrides backend/model)
    skip_permissions: false   # Pass --dangerously-skip-permissions to Claude
    depends_on: [other-agent] # Start after these agents are ready
    worktree: true            # Git worktree isolation override
    branch: feature-branch    # Custom branch name for worktree

# Messaging channel (optional)
channel:
  bot_token_env: TELEGRAM_BOT_TOKEN  # Env var for bot token
  group_id: -100123456789            # Telegram group ID
```

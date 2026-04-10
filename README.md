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
│  │   MCP Server (per agent) │    │    Tools: send_to_instance, broadcast, list_instances
│  └─────────────────────────┘    │
└─────────────────────────────────┘
```

## Quick Start

```bash
# Create fleet.yaml
cat > fleet.yaml << 'YAML'
defaults:
  backend: claude

instances:
  alice:
    skip_permissions: true
    working_directory: /tmp/alice-workspace
  bob:
    skip_permissions: true
    working_directory: /tmp/bob-workspace
YAML

# Start daemon
cargo run --bin agend-daemon

# Attach to an agent (in another terminal)
cargo run --bin agend-tui -- alice

# Detach: Ctrl+B d
# Shutdown: agend-daemon --shutdown
```

## CLI Usage

```bash
# From fleet.yaml (recommended)
agend-daemon

# From CLI args
agend-daemon alice:claude bob:bash

# Attach to agent
agend-tui alice

# Shutdown
agend-daemon --shutdown
```

## Features

- **Multi-agent PTY management** via portable-pty
- **Daemon/client split** — agents persist across TUI disconnects
- **VTerm screen state** — reconnect gets proper screen dump (via alacritty_terminal)
- **Inter-agent messaging** — MCP tools: `send_to_instance`, `broadcast`, `list_instances`
- **Auto-dismiss trust dialogs** — Claude Code trust prompt handled automatically
- **Terminal resize** — synced from TUI client to PTY
- **fleet.yaml config** — define agents, backends, working directories
- **Graceful shutdown** — Ctrl+C or `--shutdown` command

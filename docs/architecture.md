# agend-pty Architecture

A persistent PTY-based fleet manager for AI coding agents. Each agent runs inside
a pseudo-terminal managed by a long-lived daemon. Users connect via a TUI that can
attach/detach (like tmux), and agents communicate through an API socket exposed as
MCP tools.

## System Overview

```
                        fleet.yaml
                            |
                       agend-daemon
                      /     |      \
                 +----+  +----+  +----+
                 |PTY1|  |PTY2|  |PTY3|    (claude, codex, gemini, ...)
                 +----+  +----+  +----+
                   |        |       |
              tui.sock  tui.sock  tui.sock   <-- tagged binary frames
                   |
              agend-tui                       <-- Ctrl+B d detach
                   |
              user terminal

                       agend-daemon
                            |
                        api.sock              <-- newline-delimited JSON
                       /    |    \
              agend-mcp  inject   channel adapter
              (stdin/    (CLI)    (Telegram)
              stdout
              JSON-RPC)
```

**Data flow**: The daemon opens one PTY per agent, reads output through a VTerm
(alacritty_terminal), feeds a state machine for lifecycle detection, and broadcasts
raw bytes to subscribed TUI clients. Fleet coordination happens through the API
socket, which the MCP server bridges to JSON-RPC 2.0 for the agents themselves.

---

## Binaries

The project produces four binaries from a single Rust crate.

### agend-pty (CLI entry point)

**Source**: `src/main.rs`

Dispatcher with subcommands. Delegates to sibling binaries via `exec`:

| Subcommand       | Action                                        |
|------------------|-----------------------------------------------|
| `daemon`/`start` | Exec `agend-daemon`                           |
| `attach`/`a`     | Exec `agend-tui [agent-name]`                 |
| `quickstart`     | Interactive setup wizard                      |
| `doctor`         | System health check (git, backends, sockets)  |
| `dry-run`        | Validate fleet.yaml without starting agents   |
| `snapshot`       | Save fleet state to JSON                      |
| `restore`        | Restore fleet from snapshot                   |
| `list`/`ls`      | List running agents                           |
| `status`         | Show running daemons with uptime              |
| `inject`         | Send message to agent via API socket          |
| `cleanup`        | Remove leftover git worktrees                 |
| `shutdown`       | Signal daemon to stop via ctrl.sock           |

Supports symlink aliases: `agend-daemon` binary name maps to `daemon` subcommand,
`agend-tui` maps to `attach`.

### agend-daemon (persistent PTY manager)

**Source**: `src/daemon.rs`

The core process. Responsibilities:

1. Parse fleet.yaml or CLI args (`name:command` pairs)
2. Resolve dependency order via Kahn's algorithm (see Features)
3. Spawn agents layer-by-layer, waiting for each layer to reach Ready state
4. Run per-agent PTY read loops (state detection, auto-dismiss, VTerm, broadcast)
5. Start API socket server, channel poll thread, health tick thread, ctrl socket
6. Handle crash recovery via health monitor (auto-respawn with backoff)

**Key data structures**:

- `AgentCore` (Mutex-protected): VTerm instance + subscriber list. Broadcast and
  subscribe are atomic under the same lock -- no output gap when a TUI connects.
- `AgentHandle`: PTY writer, core, submit_key, inject_prefix, state machine, health.
- `SpawnConfig`: Persistent across respawns. Holds `Arc<Mutex<StateMachine>>` and
  `Arc<Mutex<HealthMonitor>>` so crash history survives restart.
- `AgentRegistry`: `HashMap<String, AgentHandle>` behind `Arc<Mutex<_>>`.

**Thread architecture** (per agent):

- `agent_{name}`: Runs `spawn_agent`, blocks on TUI socket listener
- `{name}_pty_read`: Reads PTY output, feeds state machine + VTerm + broadcast
- `{name}_tui_out`: Forwards broadcast channel to connected TUI client
- `{name}_tui_in`: Forwards TUI input to PTY writer, handles resize

**Global threads**:

- `api_server`: Accepts connections on api.sock, spawns per-connection handlers
- `channel_poll`: Polls Telegram adapter, routes messages to agent PTYs
- `health_tick`: Every 3s, drives time-based state transitions (idle detection,
  hang timeout) and backoff-gated restarts
- `ctrl_sock`: Listens for shutdown signal

### agend-tui (terminal client)

**Source**: `src/tui.rs`

Connects to an agent's `tui.sock` Unix domain socket. Features:

- Raw terminal mode with RAII guard (`RawModeGuard` -- restores on drop/panic)
- Sends initial terminal size, tracks resize events
- **Ctrl+B d** to detach (agent keeps running, like tmux/screen)
- Receives screen dump on connect (VTerm snapshot for seamless reattach)
- Supports paste events forwarded as raw bytes

### agend-mcp (MCP server bridge)

**Source**: `src/mcp.rs`

Translates between JSON-RPC 2.0 on stdin/stdout and the daemon's API socket:

1. Reads NDJSON from stdin (MCP protocol from the AI agent)
2. Forwards `tools/list` and `tools/call` to the API socket
3. Returns JSON-RPC responses on stdout

Identity via `AGEND_INSTANCE_NAME` env var. Auto-discovers API socket by scanning
`~/.agend/run/*/api.sock`, with retry loop (up to 5s for daemon startup).

Handles `initialize` locally (returns protocol version, capabilities, server info).

---

## State Machine

**Source**: `src/state.rs`

### States

```
Starting --> Ready --> Busy --> Ready (cycle)
    |          |        |        |
    |          |        +---> WaitingForInput --> Ready
    |          |        |
    |          +------> Idle --> Busy
    |
    +---> Errored --> Restarting --> Starting
    |        ^           ^
    +---> Crashed -------+
```

Eight states: `Starting`, `Ready`, `Busy`, `Idle`, `Errored`, `Crashed`,
`Restarting`, `WaitingForInput`.

### Transition Table

| From             | Event                  | To               |
|------------------|------------------------|------------------|
| Starting         | ReadyPatternDetected   | Ready            |
| Starting         | ErrorPatternDetected   | Errored          |
| Starting         | ProcessExited          | Crashed          |
| Starting         | InputRequested         | WaitingForInput  |
| Ready            | OutputReceived         | Busy             |
| Ready            | SilenceDuration(30s)   | Idle             |
| Ready            | ErrorPatternDetected   | Errored          |
| Busy             | ReadyPatternDetected   | Ready            |
| Busy             | ErrorPatternDetected   | Errored          |
| Busy             | InputRequested         | WaitingForInput  |
| Busy/Idle/Ready  | ProcessExited          | Crashed          |
| Idle             | OutputReceived         | Busy             |
| Errored          | ReadyPatternDetected   | Ready            |
| Errored          | RestartInitiated       | Restarting       |
| Crashed          | RestartInitiated       | Restarting       |
| Restarting       | RestartComplete        | Starting         |
| WaitingForInput  | ReadyPatternDetected   | Ready            |
| WaitingForInput  | OutputReceived         | Busy             |

### Directional Hysteresis

- **Escalation (to Errored)**: 2-second debounce. Error must persist through a
  `tick()` call before the state commits. Prevents false positives from transient
  error messages in output.
- **Recovery (to Ready)**: Immediate. No delay on detecting a ready pattern after
  an error -- recover as fast as possible.
- **Buffer clearing**: Detection buffer is cleared on every state transition to
  prevent stale patterns from re-triggering.

### Error Classification

`ErrorKind` classifies errors detected from PTY output:

| Kind         | Detection Patterns                              | Permanent? |
|--------------|------------------------------------------------|------------|
| RateLimit    | "rate limit", "429"                            | No         |
| AuthError    | "unauthorized", "invalid api key", "401"       | Yes        |
| ContextFull  | "context" + ("full" or "limit" or "too long")  | No         |
| ApiError     | "error:", "fatal:", "panic:", thread panic      | No         |

In `Starting` state, broader patterns match (e.g., bare "error"). In other states,
patterns require a colon suffix (e.g., "error:") to reduce false positives.

### Pattern Detection

`StatePatterns` holds per-backend patterns:
- `ready_patterns`: Pipe-delimited strings (e.g., `"Type your"` for Claude,
  `">|gemini"` for Gemini)
- `input_patterns`: Trust dialog patterns like `"yes, i trust"`, `"(y/n)"`, etc.

All matching is case-insensitive against a rolling 4KB detection buffer.

---

## Health Monitor

**Source**: `src/health.rs`

### Health States

`Healthy` --> `Degraded` (on first crash) --> `Failed` (on 3 crashes in window or
permanent error).

### Constants

| Parameter              | Value   |
|------------------------|---------|
| Initial backoff        | 5s      |
| Max backoff            | 300s    |
| Crash window           | 600s (10 min) |
| Max crashes in window  | 3       |
| Hang timeout           | 900s (15 min) |
| Max consecutive errors | 3       |

### Sliding Window Crash Detection

Crash timestamps are stored in a `Vec<Instant>`. The backoff duration is computed
from the number of crashes within the 10-minute window:

```
backoff = 5s * 2^(window_crashes - 1)    # capped at 300s
```

When the window expires with no new crashes, backoff naturally resets to 5s. Old
crash entries (>2x window) are pruned to prevent unbounded growth.

### Health Actions

| Trigger                  | Action         | Effect                          |
|--------------------------|----------------|---------------------------------|
| Single crash             | Restart        | Respawn after backoff           |
| 3 crashes in 10min       | MarkFailed     | No more restarts                |
| AuthError detected       | MarkFailed     | Permanent -- no respawn         |
| 3 consecutive errors     | MarkFailed     | Too many errors without recovery|
| 15min in Busy state      | KillAndRestart | Send Ctrl+C + EOF, respawn      |
| Ready/Idle after Degraded| (implicit)     | Restore to Healthy              |

### Respawn Flow

1. PTY read loop detects `read() == 0` (PTY closed)
2. State machine transitions to `Crashed`
3. Health monitor returns `HealthAction::Restart`
4. Agent is removed from registry (cleanup first, no race)
5. `do_respawn` spawns a new thread that calls `spawn_agent` with the same
   `SpawnConfig` (health/state monitors are preserved via Arc)

---

## Git Worktree Isolation

**Source**: `src/git.rs`

Each agent gets its own git worktree to avoid file conflicts:

- **Branch**: `agend/<agent-name>` (created from HEAD if it does not exist)
- **Path**: `<repo>/.agend/worktrees/<agent-name>`
- **Custom branch**: Optional via `branch:` in fleet.yaml

Key operations:

| Function           | Purpose                                      |
|--------------------|----------------------------------------------|
| `create_worktree`  | Create branch + worktree (reuse on respawn)  |
| `remove_worktree`  | Force-remove worktree                        |
| `list_worktrees`   | Enumerate existing agent worktrees           |
| `merge_preview`    | Diff stat + conflict detection via merge-tree|
| `squash_merge`     | Squash merge agent branch, abort on conflict |
| `cleanup_worktrees`| Remove all agent worktrees                   |

Warns if `.agend/` is not in `.gitignore`.

---

## Socket Protocol

### TUI <-> Daemon (tui.sock)

Tagged binary frames over Unix domain socket:

```
+------+--------+---------+
| tag  | length | payload |
| 1B   | 4B BE  | N bytes |
+------+--------+---------+
```

| Tag | Name       | Payload                        |
|-----|------------|--------------------------------|
| 0   | TAG_DATA   | Raw terminal bytes             |
| 1   | TAG_RESIZE | 4 bytes: cols(2B BE) + rows(2B BE) |

Max frame size: 1MB. On connect, daemon sends a VTerm screen dump as the first
DATA frame so reattach shows the current screen state.

### API Socket (api.sock)

Newline-delimited JSON. One request per line, one response per line.

Request:
```json
{"method": "inject", "params": {"instance": "alice", "message": "hello"}}
```

Response:
```json
{"ok": true, "result": {"sent": true}}
```

Methods: `list`, `status`, `inject`, `kill`, `mcp_call`, `mcp_tools_list`.

### MCP Protocol (stdin/stdout)

JSON-RPC 2.0 over stdin/stdout (NDJSON). The `agend-mcp` binary acts as a
bridge: it receives JSON-RPC from the AI agent, translates to API socket calls,
and returns JSON-RPC responses.

Supported JSON-RPC methods: `initialize`, `tools/list`, `tools/call`,
`notifications/initialized` (no-op).

---

## Data Persistence

**Source**: `src/util.rs`

All persistent data uses append-only JSONL files. The `util` module provides two
shared functions used by every store:

- `read_jsonl<T>(path)`: Read all lines, skip parse errors, return `Vec<T>`
- `append_jsonl<T>(path, item)`: Append one JSON line (creates parent dirs)

### File Locations

```
~/.agend/
  run/<pid>/
    daemon.lock          # flock-based daemon lock
    api.sock             # API socket
    ctrl.sock            # Shutdown control socket
    events.jsonl         # Event log (state changes, health actions, PTY close)
    schedules.jsonl      # Cron schedules
    decisions.jsonl      # Fleet-wide decisions
    tasks.jsonl          # Task board
    teams.jsonl          # Team definitions
    inbox/
      <agent>.jsonl      # Per-agent message queue
    agents/
      <agent>/
        tui.sock         # TUI socket
        mcp-config.json  # Auto-generated MCP server config
        prompt.md        # Auto-generated fleet context prompt
  topics.json            # Telegram topic ID mappings
```

---

## MCP Tools

23 tools exposed to agents via the MCP server:

### Communication
| Tool                 | Description                                    |
|----------------------|------------------------------------------------|
| `reply`              | Reply to a Telegram/Discord user               |
| `send_to_instance`   | Send message to another agent                  |
| `request_information` | Ask another agent a question                  |
| `delegate_task`      | Delegate work to another agent with criteria   |
| `report_result`      | Return results with correlation ID             |
| `broadcast`          | Send to all agents (or team subset)            |
| `react`              | React to a message with emoji                  |
| `edit_message`       | Edit a previously sent message                 |

### Fleet Discovery
| Tool                 | Description                                    |
|----------------------|------------------------------------------------|
| `list_instances`     | List running agent instances                   |
| `describe_instance`  | Get agent details and status                   |
| `create_instance`    | Spawn a new agent at runtime                   |
| `delete_instance`    | Stop agent (optionally cleanup worktree)       |
| `replace_instance`   | Atomic swap: replace agent with new settings   |
| `wait_for_idle`      | Block until agent reaches Ready/Idle (timeout) |

### Coordination
| Tool                 | Description                                    |
|----------------------|------------------------------------------------|
| `inbox`              | Read inbox messages (list or by ID)            |
| `post_decision`      | Post a fleet-wide decision record              |
| `list_decisions`     | List all fleet decisions                       |
| `update_decision`    | Update an existing decision                    |
| `task`               | Task board CRUD (create/list/claim/done/update)|
| `team`               | Team CRUD (create/list/delete/update)          |
| `list_events`        | Query event log (filter by agent/type)         |
| `schedule`           | Cron schedule CRUD (create/list/delete/update) |

### Git Integration
| Tool                 | Description                                    |
|----------------------|------------------------------------------------|
| `merge_preview`      | Preview merge diff + conflict detection        |
| `merge_agent`        | Squash merge agent branch into main            |
| `merge_all`          | Batch squash merge all agent branches           |
| `watch_ci`           | Check GitHub PR CI status via gh CLI            |

---

## Fleet Configuration (fleet.yaml)

```yaml
defaults:
  backend: claude          # Default backend for all instances
  model: opus-4             # Optional default model
  working_directory: /path  # Optional default working dir
  worktree: true           # Git worktree isolation (default: true)

channel:
  bot_token_env: TELEGRAM_BOT_TOKEN   # Env var name for bot token
  group_id: -100123456789             # Telegram group ID

instances:
  alice:
    backend: claude              # Override default backend
    model: sonnet-4              # Override model
    working_directory: /path     # Override working dir
    skip_permissions: true       # Add --dangerously-skip-permissions
    worktree: true               # Override worktree setting
    branch: custom-branch        # Custom git branch name
    command: "custom cmd"        # Full command override
    depends_on: [bob]            # Start after bob is Ready

  bob:
    skip_permissions: true
    working_directory: /path
```

Config search order: `./fleet.yaml`, `./fleet.yml`, `~/.agend/fleet.yaml`.

`depends_on` creates a DAG resolved by Kahn's algorithm (`features.rs`). Agents
are grouped into parallel layers; layer N+1 waits up to 60s for layer N to reach
Ready/Busy/Idle.

---

## Backend Presets

**Source**: `src/backend.rs`

Five supported backends with auto-detection from command name:

| Backend      | Command    | Ready Pattern   | Submit Key |
|--------------|------------|-----------------|------------|
| ClaudeCode   | `claude`   | `"Type your"`   | `\r`       |
| KiroCli      | `kiro-cli` | `"ready\|chat\|>"` | `\r`    |
| Codex        | `codex`    | `">\|codex"`    | `\r`       |
| OpenCode     | `opencode` | `"opencode\|>"` | `\r`       |
| Gemini       | `gemini`   | `">\|gemini"`   | `\n\r`     |

Each backend defines `dismiss_patterns` for auto-dismissing trust dialogs (capped
at 5 per session). Claude uses `--mcp-config` for MCP injection; others use
file-based config. Gemini uses `typed_inject` for character-by-character input.

---

## Other Subsystems

| Module | Source | Purpose |
|--------|--------|---------|
| **Channel Adapter** | `channel.rs`, `telegram.rs` | Abstract `ChannelAdapter` trait. Telegram: forum topics per agent, Bot API via `isahc`. |
| **Virtual Terminal** | `vterm.rs` | `alacritty_terminal` wrapper. Screen dump for TUI reattach, resize support. |
| **Inbox** | `inbox.rs` | Short msgs (<=500 chars) injected directly; long msgs stored in JSONL with notification. |
| **Scheduler** | `scheduler.rs` | Cron-based scheduling (`cron` crate). Tick thread checks due schedules. |
| **Snapshot/Restore** | `features.rs` | `FleetSnapshot` captures fleet.yaml + agent list + topic mappings. |
| **Instructions** | `instructions.rs` | Auto-generates backend-specific instruction files in working dirs. |

---

## Key Design Decisions

1. **Single Mutex per concern** — AgentCore locks VTerm + subscribers together; subscribe under lock guarantees no output gap on TUI reattach.
2. **Arc-shared monitors survive respawn** — StateMachine/HealthMonitor in `Arc<Mutex<_>>` carry crash history across restarts.
3. **Append-only JSONL** — Updates append new versions; reads dedup by ID. No file locking needed.
4. **Tagged binary framing** — TUI uses tag+length+payload, avoiding escaping issues with raw terminal bytes.
5. **Poison-resistant locks** — All Mutex ops use `unwrap_or_else(|e| e.into_inner())` to recover rather than panic.

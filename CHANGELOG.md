# Changelog

## [0.5.0] - 2026-04-12

### Added
- 23 MCP tools (communication, fleet discovery, coordination, git, CI)
- Git worktree isolation (per-agent branches, merge_preview, merge_agent, merge_all)
- Dependency graph with Kahn's algorithm (layered startup, depends_on)
- 8-state lifecycle machine with directional hysteresis and ErrorKind classification
- Health monitor (auto-respawn, exponential backoff, sliding window crash detection, hang detection)
- Session timer (max_session_hours with 80% warning, 100% auto-stop)
- Cron scheduler (CRUD via MCP tools)
- Teams, Decisions, Task board (fleet-wide coordination)
- Telegram channel adapter (forum topics, react, edit_message, reply_to)
- Quickstart wizard (interactive fleet.yaml setup with env scanning)
- Demo mode (2 echo agents, zero API key needed)
- Bugreport export (one-click diagnostic dump with token redaction)
- Live dashboard (status --live, ASCII fleet overview with 2s polling)
- Read-only log streaming (agend-pty logs, similar to docker logs -f)
- Event log (structured append-only JSONL)
- Structured logging via tracing (AGEND_LOG env filter)
- MCP socket pooling (daemon-native JSON-RPC, identity-injecting bridge)
- CI status checking (watch_ci via gh CLI)
- replace_instance (atomic agent swap with health reset)
- Architecture documentation (docs/architecture.md, 500 lines)
- 4 example fleet.yaml files (basic, multi-agent, pipeline, telegram)
- Enhanced doctor health checks (fleet validation, Telegram token verify, worktree scan)
- 9 E2E integration tests

### Changed
- Version bump from 0.1.0 to 0.5.0
- Shared JSONL utilities (util.rs: now_secs, read_jsonl, append_jsonl)
- Backend preset cleanup (removed unused fields)
- resolve_backend_binary unified in config.rs
- CLI error messages with actionable hints

### Removed
- agend-mcp-bridge binary (merged into agend-mcp)
- tools_list_fallback (error on API failure instead)
- find_agent_mcp_socket (no longer needed)

## [0.1.0] - 2026-04-10

### Added
- Initial PTY-based agent fleet manager
- Daemon/client architecture (agend-daemon, agend-tui)
- Basic PTY management with portable-pty
- VTerm screen state via alacritty_terminal
- Inter-agent messaging (send_to_instance, broadcast, list_instances)
- Auto-dismiss trust dialogs (Claude, Codex, Gemini)
- Terminal resize sync
- fleet.yaml configuration
- Graceful shutdown (Ctrl+C, --shutdown)

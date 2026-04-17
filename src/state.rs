//! Agent state detection via PTY output pattern matching.
//!
//! 14 fine-grained states, priority-ordered. Errors ARE states (not metadata),
//! which lets health policy dispatch on them directly.
//!
//! Hysteresis rules:
//! - Error states (instant): any error pattern → instant transition (no debounce)
//! - Higher priority than current (instant): e.g. Idle → Thinking
//! - Lower priority (hold): e.g. Thinking → Idle needs 2s active hold
//! - Passive-to-passive (hold): Idle → Ready needs 5s passive hold
//! - Buffer cleared on every transition to prevent stale pattern re-trigger

use crate::backend::Backend;
use regex::Regex;
use std::time::{Duration, Instant};

const STATE_BUF_MAX: usize = 4096;
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const ACTIVE_HOLD: Duration = Duration::from_secs(2);
const PASSIVE_HOLD: Duration = Duration::from_secs(5);
/// Starting must show progress within 2 min or considered hung.
const STARTING_HANG_TIMEOUT: Duration = Duration::from_secs(120);
/// Active work (Thinking/ToolUse) can legitimately run long but 10 min of silence = hang.
const WORK_HANG_TIMEOUT: Duration = Duration::from_secs(600);

/// Agent runtime state. Priority is encoded as the discriminant order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Starting,
    Hang,
    Ready,
    Idle,
    ToolUse,
    Thinking,
    PermissionPrompt,
    ContextFull,
    RateLimit,
    UsageLimit,
    AuthError,
    ApiError,
    Crashed,
    Restarting,
}

impl AgentState {
    /// Priority: higher = more urgent. Error states dominate; unavailable states are highest.
    pub fn priority(self) -> u8 {
        match self {
            Self::Starting => 0,
            Self::Hang => 1,
            Self::Ready => 2,
            Self::Idle => 3,
            Self::ToolUse => 4,
            Self::Thinking => 5,
            Self::PermissionPrompt => 6,
            Self::ContextFull => 7,
            Self::RateLimit => 8,
            Self::UsageLimit => 9,
            Self::AuthError => 10,
            Self::ApiError => 11,
            Self::Crashed => 12,
            Self::Restarting => 13,
        }
    }

    /// Error states transition instantly (no hysteresis).
    pub fn is_error(self) -> bool {
        self.priority() >= Self::ContextFull.priority()
    }

    /// Permanent errors must not be auto-respawned (AuthError).
    pub fn is_permanent_error(self) -> bool {
        matches!(self, Self::AuthError)
    }

    /// Agent unavailable (not usable until respawn completes).
    pub fn is_unavailable(self) -> bool {
        matches!(self, Self::Crashed | Self::Restarting)
    }

    /// Waiting on user input (permission prompt).
    pub fn is_waiting_input(self) -> bool {
        matches!(self, Self::PermissionPrompt)
    }

    /// Actively working (legitimately busy).
    pub fn is_working(self) -> bool {
        matches!(self, Self::Thinking | Self::ToolUse)
    }

    /// Passive (accepting input, not actively working).
    pub fn is_passive(self) -> bool {
        matches!(self, Self::Ready | Self::Idle)
    }

    /// Agent is live (not error, not unavailable, not still starting).
    /// Used for dependency-layer wait conditions.
    pub fn is_live(self) -> bool {
        self.is_passive() || self.is_working() || self.is_waiting_input()
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Hang => "hang",
            Self::Ready => "ready",
            Self::Idle => "idle",
            Self::ToolUse => "tool_use",
            Self::Thinking => "thinking",
            Self::PermissionPrompt => "permission_prompt",
            Self::ContextFull => "context_full",
            Self::RateLimit => "rate_limit",
            Self::UsageLimit => "usage_limit",
            Self::AuthError => "auth_error",
            Self::ApiError => "api_error",
            Self::Crashed => "crashed",
            Self::Restarting => "restarting",
        }
    }
}

/// Compiled patterns for one backend, in priority order (highest priority first).
pub struct StatePatterns {
    patterns: Vec<(AgentState, Regex)>,
}

impl StatePatterns {
    /// Backend-specific regex patterns. Sources annotated: [measured], [docs], [estimated].
    pub fn for_backend(backend: &Backend) -> Self {
        let raw: Vec<(AgentState, &str)> = match backend {
            // Claude Code
            Backend::ClaudeCode => vec![
                (
                    AgentState::AuthError,
                    r"API key|authentication failed|unauthorized|invalid api key",
                ),
                (AgentState::RateLimit, r"overloaded|rate.?limit|\b429\b"),
                (
                    AgentState::ContextFull,
                    r"compacting context|context.*(full|limit|too long)",
                ),
                (
                    AgentState::PermissionPrompt,
                    r"Allow once|Allow always|Yes, I trust|Yes, proceed|approve|permission required",
                ),
                (AgentState::Thinking, r"Thinking"),
                (
                    AgentState::ToolUse,
                    r"[\u{280b}\u{2819}\u{2839}\u{2838}\u{283c}\u{2834}\u{2826}\u{2827}\u{2807}\u{280f}\u{2713}\u{25cf}].*(Read|Bash|Edit|Write|Grep|Glob)",
                ),
                (AgentState::Idle, r"\u{276f}"),
                (AgentState::Ready, r"bypass permissions|Type your"),
            ],
            // Kiro CLI
            Backend::KiroCli => vec![
                (
                    AgentState::AuthError,
                    r"Not authenticated|AccessDenied|denied access",
                ),
                (
                    AgentState::UsageLimit,
                    r"ServiceQuotaExceeded|InsufficientModelCapacity",
                ),
                (
                    AgentState::RateLimit,
                    r"Too Many Requests|ThrottlingError|\b429\b",
                ),
                (AgentState::ContextFull, r"context window overflow|/compact"),
                (AgentState::PermissionPrompt, r"Allow this action|y/n/t"),
                (AgentState::Thinking, r"Generating"),
                (AgentState::ToolUse, r"execute_bash|fs_read|fs_write"),
                (
                    AgentState::Idle,
                    r"\d+%\s*$|ask a question or describe a task",
                ),
                (AgentState::Ready, r"Trust All Tools active|/quit to exit"),
            ],
            // Codex
            Backend::Codex => vec![
                (AgentState::AuthError, r"OPENAI_API_KEY|api.?key"),
                (AgentState::UsageLimit, r"hit your usage limit|try again at"),
                (AgentState::RateLimit, r"rate.?limit|\b429\b"),
                (AgentState::ContextFull, r"ContextOverflow"),
                (
                    AgentState::PermissionPrompt,
                    r"Request approval|approve|deny|Do you trust|Yes, continue",
                ),
                (AgentState::Thinking, r"Thinking"),
                (AgentState::ToolUse, r"apply_patch"),
                (AgentState::Idle, r"\u{203a}"),
                (AgentState::Ready, r"OpenAI Codex|gpt-.*left"),
            ],
            // OpenCode
            Backend::OpenCode => vec![
                (AgentState::RateLimit, r"rate.?limit|\b429\b"),
                (AgentState::ContextFull, r"ContextOverflow"),
                (
                    AgentState::PermissionPrompt,
                    r"Permission required|Allow once|Allow always|Update Available|Skip\s+Confirm",
                ),
                (AgentState::Thinking, r"Working"),
                (AgentState::Idle, r"Ask anything"),
                (AgentState::Ready, r"Ask anything|tab agents"),
            ],
            // Gemini
            Backend::Gemini => vec![
                (
                    AgentState::AuthError,
                    r"OAuth not authenticated|OAuth expired|UNAUTHENTICATED|check API key",
                ),
                (
                    AgentState::UsageLimit,
                    r"Usage limit reached|Access resets at",
                ),
                (AgentState::RateLimit, r"RESOURCE_EXHAUSTED|\b429\b"),
                (AgentState::ContextFull, r"quota.*exceeded|token.*limit"),
                (
                    AgentState::PermissionPrompt,
                    r"Allow once|Allow for this session|suggest changes|Trust folder|Don't trust",
                ),
                (AgentState::Thinking, r"Thinking"),
                (AgentState::ToolUse, r"tool.*call|MCP.*tool"),
                (AgentState::Idle, r"Type your message"),
                (AgentState::Ready, r"Type your message|YOLO"),
            ],
        };
        Self::compile(raw)
    }

    /// Fallback for unknown backends — matches ready/idle via supplied pipe-separated pattern.
    /// Used when Backend cannot be inferred from command.
    pub fn fallback(ready_pattern: &str) -> Self {
        let alternatives: Vec<String> = ready_pattern
            .split('|')
            .map(regex::escape)
            .filter(|s| !s.is_empty())
            .collect();
        let combined = alternatives.join("|");
        let raw: Vec<(AgentState, String)> = if combined.is_empty() {
            Vec::new()
        } else {
            vec![
                (AgentState::Ready, format!("(?i){combined}")),
                (
                    AgentState::ApiError,
                    r"(?i)error:|fatal:|panic:|thread.*panicked".into(),
                ),
            ]
        };
        let compiled = raw
            .into_iter()
            .filter_map(|(state, pat)| match Regex::new(&pat) {
                Ok(re) => Some((state, re)),
                Err(err) => {
                    tracing::warn!("invalid fallback state pattern: {pat}: {err}");
                    None
                }
            })
            .collect();
        Self { patterns: compiled }
    }

    fn compile(raw: Vec<(AgentState, &str)>) -> Self {
        let patterns = raw
            .into_iter()
            .filter_map(|(state, pat)| match Regex::new(pat) {
                Ok(re) => Some((state, re)),
                Err(err) => {
                    tracing::warn!("invalid state pattern: {pat}: {err}");
                    None
                }
            })
            .collect();
        Self { patterns }
    }

    /// Match against buffer, return highest-priority matching state.
    pub fn detect(&self, text: &str) -> Option<AgentState> {
        // Patterns are stored in priority order (highest first).
        for (state, re) in &self.patterns {
            if re.is_match(text) {
                return Some(*state);
            }
        }
        None
    }
}

/// Tracks state with priority-based hysteresis.
pub struct StateMachine {
    state: AgentState,
    since: Instant,
    last_output: Instant,
    detect_buf: String,
    patterns: StatePatterns,
    consecutive_errors: u32,
}

impl StateMachine {
    pub fn new(patterns: StatePatterns) -> Self {
        let now = Instant::now();
        Self {
            state: AgentState::Starting,
            since: now,
            last_output: now,
            detect_buf: String::with_capacity(STATE_BUF_MAX),
            patterns,
            consecutive_errors: 0,
        }
    }

    pub fn state(&self) -> AgentState {
        self.state
    }

    pub fn consecutive_errors(&self) -> u32 {
        self.consecutive_errors
    }

    /// Feed stripped PTY output. Returns new state if changed.
    pub fn process_output(&mut self, clean_text: &str, now: Instant) -> Option<AgentState> {
        if clean_text.is_empty() {
            return None;
        }
        self.last_output = now;
        self.detect_buf.push_str(clean_text);
        if self.detect_buf.len() > STATE_BUF_MAX {
            let mut start = self.detect_buf.len() - STATE_BUF_MAX;
            while !self.detect_buf.is_char_boundary(start) {
                start += 1;
            }
            self.detect_buf = self.detect_buf[start..].to_string();
        }

        if let Some(detected) = self.patterns.detect(&self.detect_buf) {
            self.try_transition(detected, now)
        } else {
            None
        }
    }

    /// Periodic tick for time-based transitions (Idle after silence, Hang detection).
    pub fn tick(&mut self, now: Instant) -> Option<AgentState> {
        // Hang detection from Starting
        if self.state == AgentState::Starting
            && now.duration_since(self.last_output) >= STARTING_HANG_TIMEOUT
        {
            return self.try_transition(AgentState::Hang, now);
        }
        // Hang detection from active work
        if self.state.is_working()
            && now.duration_since(self.last_output) >= WORK_HANG_TIMEOUT
        {
            return self.try_transition(AgentState::Hang, now);
        }
        // Idle from Ready after silence (fallback when no idle pattern is defined)
        if self.state == AgentState::Ready
            && now.duration_since(self.last_output) >= IDLE_TIMEOUT
        {
            return self.try_transition(AgentState::Idle, now);
        }
        None
    }

    pub fn on_exit(&mut self, now: Instant) -> Option<AgentState> {
        self.force_transition(AgentState::Crashed, now)
    }

    pub fn on_restart(&mut self, now: Instant) -> Option<AgentState> {
        self.force_transition(AgentState::Restarting, now)
    }

    pub fn on_restart_complete(&mut self, now: Instant) -> Option<AgentState> {
        self.consecutive_errors = 0;
        self.force_transition(AgentState::Starting, now)
    }

    /// Hysteresis-aware transition attempt.
    fn try_transition(&mut self, target: AgentState, now: Instant) -> Option<AgentState> {
        if target == self.state {
            return None;
        }
        // Errors always transition instantly.
        if target.is_error() {
            return self.apply(target, now);
        }
        // Higher priority always transitions instantly.
        if target.priority() > self.state.priority() {
            return self.apply(target, now);
        }
        // Lower priority: require hold time on current state before stepping down.
        let held = now.saturating_duration_since(self.since);
        let required = if self.state.is_passive() {
            PASSIVE_HOLD
        } else {
            ACTIVE_HOLD
        };
        if held >= required {
            self.apply(target, now)
        } else {
            None
        }
    }

    fn force_transition(&mut self, target: AgentState, now: Instant) -> Option<AgentState> {
        if target == self.state {
            return None;
        }
        self.apply(target, now)
    }

    fn apply(&mut self, target: AgentState, now: Instant) -> Option<AgentState> {
        if target.is_error() {
            self.consecutive_errors = self.consecutive_errors.saturating_add(1);
        }
        if target.is_passive() {
            self.consecutive_errors = 0;
        }
        self.state = target;
        self.since = now;
        self.detect_buf.clear();
        Some(target)
    }
}

/// Strip ANSI escape sequences from text.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    let mut params = String::new();
                    let mut final_char = ' ';
                    while let Some(&ch) = chars.peek() {
                        chars.next();
                        if ch.is_ascii_alphabetic() {
                            final_char = ch;
                            break;
                        }
                        params.push(ch);
                    }
                    // CSI C = cursor forward → replace with space
                    if final_char == 'C' {
                        let n = params.parse::<usize>().unwrap_or(1);
                        for _ in 0..n {
                            out.push(' ');
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(&ch) = chars.peek() {
                        chars.next();
                        if ch == '\x07' || ch == '\\' {
                            break;
                        }
                    }
                }
                Some('(') | Some(')') => {
                    chars.next();
                    chars.next();
                }
                _ => {
                    chars.next();
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker_at(backend: &Backend, state: AgentState, elapsed_secs: u64) -> StateMachine {
        let mut t = StateMachine::new(StatePatterns::for_backend(backend));
        t.state = state;
        t.since = Instant::now() - Duration::from_secs(elapsed_secs);
        t.last_output = t.since;
        t
    }

    // ── Priority & classification ───────────────────────────────────────

    #[test]
    fn all_errors_flagged() {
        for s in [
            AgentState::ContextFull,
            AgentState::RateLimit,
            AgentState::UsageLimit,
            AgentState::AuthError,
            AgentState::ApiError,
        ] {
            assert!(s.is_error(), "{:?} should be error", s);
        }
        for s in [
            AgentState::Ready,
            AgentState::Idle,
            AgentState::Thinking,
            AgentState::ToolUse,
            AgentState::Starting,
            AgentState::Hang,
            AgentState::PermissionPrompt,
        ] {
            assert!(!s.is_error(), "{:?} should not be error", s);
        }
    }

    #[test]
    fn only_auth_error_is_permanent() {
        assert!(AgentState::AuthError.is_permanent_error());
        for s in [
            AgentState::RateLimit,
            AgentState::ApiError,
            AgentState::ContextFull,
            AgentState::UsageLimit,
        ] {
            assert!(!s.is_permanent_error());
        }
    }

    #[test]
    fn unavailable_states() {
        assert!(AgentState::Crashed.is_unavailable());
        assert!(AgentState::Restarting.is_unavailable());
        assert!(!AgentState::Ready.is_unavailable());
        assert!(!AgentState::AuthError.is_unavailable());
    }

    #[test]
    fn live_excludes_starting_errors_unavailable() {
        assert!(AgentState::Ready.is_live());
        assert!(AgentState::Idle.is_live());
        assert!(AgentState::Thinking.is_live());
        assert!(AgentState::ToolUse.is_live());
        assert!(AgentState::PermissionPrompt.is_live());
        assert!(!AgentState::Starting.is_live());
        assert!(!AgentState::Crashed.is_live());
        assert!(!AgentState::AuthError.is_live());
    }

    #[test]
    fn priority_is_strictly_increasing() {
        let ordered = [
            AgentState::Starting,
            AgentState::Hang,
            AgentState::Ready,
            AgentState::Idle,
            AgentState::ToolUse,
            AgentState::Thinking,
            AgentState::PermissionPrompt,
            AgentState::ContextFull,
            AgentState::RateLimit,
            AgentState::UsageLimit,
            AgentState::AuthError,
            AgentState::ApiError,
            AgentState::Crashed,
            AgentState::Restarting,
        ];
        for pair in ordered.windows(2) {
            assert!(
                pair[0].priority() < pair[1].priority(),
                "priority not strictly increasing: {:?} -> {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    // ── Hysteresis rules ────────────────────────────────────────────────

    #[test]
    fn error_instant_transition() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
        let now = Instant::now();
        t.process_output("HTTP 429 rate limit exceeded", now);
        assert_eq!(t.state(), AgentState::RateLimit);
    }

    #[test]
    fn higher_priority_instant() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
        let now = Instant::now();
        // Thinking (5) > Idle (3) — instant
        t.process_output("Thinking", now);
        assert_eq!(t.state(), AgentState::Thinking);
    }

    #[test]
    fn lower_priority_needs_active_hold() {
        let backend = Backend::ClaudeCode;
        // Active (Thinking) held 1s, proposed Idle — should stay
        let mut t = tracker_at(&backend, AgentState::Thinking, 1);
        let now = Instant::now();
        t.try_transition(AgentState::Idle, now);
        assert_eq!(t.state(), AgentState::Thinking);

        // Active held 3s — should transition
        let mut t = tracker_at(&backend, AgentState::Thinking, 3);
        t.try_transition(AgentState::Idle, now);
        assert_eq!(t.state(), AgentState::Idle);
    }

    #[test]
    fn lower_priority_needs_passive_hold() {
        let backend = Backend::ClaudeCode;
        // Idle (passive) held 3s, proposed Ready — should stay
        let mut t = tracker_at(&backend, AgentState::Idle, 3);
        let now = Instant::now();
        t.try_transition(AgentState::Ready, now);
        assert_eq!(t.state(), AgentState::Idle);

        // Passive held 6s — should transition
        let mut t = tracker_at(&backend, AgentState::Idle, 6);
        t.try_transition(AgentState::Ready, now);
        assert_eq!(t.state(), AgentState::Ready);
    }

    #[test]
    fn error_dominates_active() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Thinking, 0);
        let now = Instant::now();
        t.process_output("API key invalid", now);
        assert_eq!(t.state(), AgentState::AuthError);
    }

    #[test]
    fn same_state_no_transition() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Thinking, 5);
        let since_before = t.since;
        let now = Instant::now();
        let r = t.try_transition(AgentState::Thinking, now);
        assert_eq!(r, None);
        assert_eq!(t.since, since_before);
    }

    // ── Buffer management ───────────────────────────────────────────────

    #[test]
    fn buffer_cleared_on_transition() {
        let mut t = StateMachine::new(StatePatterns::for_backend(&Backend::ClaudeCode));
        t.process_output("bypass permissions", Instant::now());
        assert_eq!(t.state(), AgentState::Ready);
        assert!(t.detect_buf.is_empty());
    }

    #[test]
    fn buffer_truncates_to_max() {
        let mut t = StateMachine::new(StatePatterns::for_backend(&Backend::ClaudeCode));
        let big = "x".repeat(STATE_BUF_MAX + 500);
        t.process_output(&big, Instant::now());
        assert!(t.detect_buf.len() <= STATE_BUF_MAX);
    }

    // ── Consecutive errors ──────────────────────────────────────────────

    #[test]
    fn consecutive_errors_increment() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
        let now = Instant::now();
        t.process_output("HTTP 429", now);
        assert_eq!(t.consecutive_errors(), 1);
    }

    #[test]
    fn consecutive_errors_reset_on_passive() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
        let now = Instant::now();
        t.process_output("HTTP 429", now);
        assert_eq!(t.consecutive_errors(), 1);
        // Transition to passive resets counter
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::RateLimit, 10);
        t.consecutive_errors = 2;
        t.process_output("bypass permissions", Instant::now());
        assert_eq!(t.state(), AgentState::Ready);
        assert_eq!(t.consecutive_errors(), 0);
    }

    // ── Lifecycle hooks ─────────────────────────────────────────────────

    #[test]
    fn on_exit_to_crashed() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Ready, 0);
        assert_eq!(t.on_exit(Instant::now()), Some(AgentState::Crashed));
    }

    #[test]
    fn restart_cycle() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Crashed, 0);
        let now = Instant::now();
        t.on_restart(now);
        assert_eq!(t.state(), AgentState::Restarting);
        t.on_restart_complete(now);
        assert_eq!(t.state(), AgentState::Starting);
        assert_eq!(t.consecutive_errors(), 0);
    }

    // ── Time-based tick ─────────────────────────────────────────────────

    #[test]
    fn starting_hangs_after_timeout() {
        let mut t = StateMachine::new(StatePatterns::for_backend(&Backend::ClaudeCode));
        let start = Instant::now();
        t.since = start;
        t.last_output = start;
        let later = start + STARTING_HANG_TIMEOUT + Duration::from_secs(1);
        assert_eq!(t.tick(later), Some(AgentState::Hang));
    }

    #[test]
    fn idle_never_hangs_via_tick() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
        t.last_output = Instant::now() - Duration::from_secs(10_000);
        assert!(t.tick(Instant::now()).is_none());
    }

    #[test]
    fn working_hangs_after_timeout() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::ToolUse, 0);
        let start = Instant::now();
        t.since = start;
        t.last_output = start;
        let later = start + WORK_HANG_TIMEOUT + Duration::from_secs(1);
        assert_eq!(t.tick(later), Some(AgentState::Hang));
    }

    #[test]
    fn ready_drifts_to_idle_after_silence() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Ready, 0);
        let start = Instant::now();
        t.since = start;
        t.last_output = start;
        let later = start + IDLE_TIMEOUT + Duration::from_secs(1);
        assert_eq!(t.tick(later), Some(AgentState::Idle));
    }

    // ── Pattern matching ────────────────────────────────────────────────

    #[test]
    fn claude_ready_pattern() {
        let mut t = StateMachine::new(StatePatterns::for_backend(&Backend::ClaudeCode));
        t.process_output("bypass permissions", Instant::now());
        assert_eq!(t.state(), AgentState::Ready);
    }

    #[test]
    fn codex_idle_pattern() {
        let mut t = tracker_at(&Backend::Codex, AgentState::Ready, 6);
        t.process_output("\u{203a}", Instant::now());
        assert_eq!(t.state(), AgentState::Idle);
    }

    #[test]
    fn gemini_auth_error() {
        let mut t = tracker_at(&Backend::Gemini, AgentState::Idle, 0);
        t.process_output("UNAUTHENTICATED: check API key", Instant::now());
        assert_eq!(t.state(), AgentState::AuthError);
    }

    #[test]
    fn fallback_matches_ready_pattern() {
        let patterns = StatePatterns::fallback(">|custom-prompt");
        assert_eq!(patterns.detect("custom-prompt>"), Some(AgentState::Ready));
    }

    #[test]
    fn fallback_empty_has_no_patterns() {
        let patterns = StatePatterns::fallback("");
        assert!(patterns.detect("anything").is_none());
    }

    #[test]
    fn fallback_detects_api_error() {
        let patterns = StatePatterns::fallback(">");
        assert_eq!(
            patterns.detect("error: something bad"),
            Some(AgentState::ApiError)
        );
    }

    #[test]
    fn display_name_stable() {
        assert_eq!(AgentState::Starting.display_name(), "starting");
        assert_eq!(AgentState::AuthError.display_name(), "auth_error");
        assert_eq!(AgentState::ToolUse.display_name(), "tool_use");
    }

    #[test]
    fn empty_input_no_transition() {
        let mut t = StateMachine::new(StatePatterns::for_backend(&Backend::ClaudeCode));
        assert!(t.process_output("", Instant::now()).is_none());
        assert_eq!(t.state(), AgentState::Starting);
    }

    // ── strip_ansi ──────────────────────────────────────────────────────

    #[test]
    fn strip_ansi_removes_color() {
        assert_eq!(strip_ansi("\x1b[32mHello\x1b[0m"), "Hello");
    }

    #[test]
    fn strip_ansi_removes_osc() {
        assert_eq!(strip_ansi("\x1b]0;title\x07text"), "text");
    }

    #[test]
    fn strip_ansi_cursor_forward_becomes_space() {
        assert_eq!(strip_ansi("a\x1b[3Cb"), "a   b");
    }
}

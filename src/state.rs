//! Agent lifecycle state machine — tracks agent state based on PTY output patterns.
//!
//! Each backend has different ready/error patterns. State transitions require
//! hysteresis (sustained duration) to prevent flapping.

use std::time::{Duration, Instant};

/// Agent lifecycle states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Starting,
    Ready,
    Busy,
    Idle,
    Errored,
    Crashed,
    Restarting,
}

/// Events that drive state transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateEvent {
    ReadyPatternDetected,
    ErrorPatternDetected,
    OutputReceived,
    SilenceDuration(Duration),
    ProcessExited,
    RestartInitiated,
    RestartComplete,
}

/// Per-backend patterns for state detection.
#[derive(Clone)]
pub struct StatePatterns {
    pub ready_patterns: Vec<String>,
    pub error_patterns: Vec<String>,
}

impl StatePatterns {
    /// Build from a backend's ready_pattern string (pipe-separated alternatives).
    pub fn from_backend(ready_pattern: &str) -> Self {
        Self {
            ready_patterns: ready_pattern.split('|').map(|s| s.to_lowercase()).collect(),
            error_patterns: vec![
                "error".into(), "fatal".into(), "panic".into(),
                "segfault".into(), "killed".into(),
            ],
        }
    }
}

/// Hysteresis config — how long a condition must hold before transition confirms.
const READY_HYSTERESIS: Duration = Duration::from_millis(500);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const BUSY_HYSTERESIS: Duration = Duration::from_millis(200);

/// Tracks agent state with hysteresis (debounce).
pub struct StateMachine {
    state: AgentState,
    patterns: StatePatterns,
    /// When the pending transition was first detected.
    pending: Option<(AgentState, Instant)>,
    last_output: Instant,
    consecutive_errors: u32,
}

impl StateMachine {
    pub fn new(patterns: StatePatterns) -> Self {
        Self {
            state: AgentState::Starting,
            patterns,
            pending: None,
            last_output: Instant::now(),
            consecutive_errors: 0,
        }
    }

    pub fn state(&self) -> AgentState { self.state }
    pub fn consecutive_errors(&self) -> u32 { self.consecutive_errors }

    /// Pure transition logic — returns new state if valid, None if invalid.
    pub fn transition(current: AgentState, event: &StateEvent) -> Option<AgentState> {
        use AgentState::*;
        use StateEvent::*;
        match (current, event) {
            (Starting, ReadyPatternDetected) => Some(Ready),
            (Starting, ErrorPatternDetected) => Some(Errored),
            (Starting, ProcessExited) => Some(Crashed),

            (Ready, OutputReceived) => Some(Busy),
            (Ready, SilenceDuration(_)) => Some(Idle),
            (Ready, ErrorPatternDetected) => Some(Errored),
            (Ready, ProcessExited) => Some(Crashed),

            (Busy, ReadyPatternDetected) => Some(Ready),
            (Busy, ErrorPatternDetected) => Some(Errored),
            (Busy, ProcessExited) => Some(Crashed),

            (Idle, OutputReceived) => Some(Busy),
            (Idle, ErrorPatternDetected) => Some(Errored),
            (Idle, ProcessExited) => Some(Crashed),

            (Errored, ReadyPatternDetected) => Some(Ready),
            (Errored, ProcessExited) => Some(Crashed),
            (Errored, RestartInitiated) => Some(Restarting),

            (Crashed, RestartInitiated) => Some(Restarting),

            (Restarting, RestartComplete) => Some(Starting),

            _ => None,
        }
    }

    /// Feed stripped (no ANSI) PTY output text. Returns new state if changed.
    pub fn process_output(&mut self, clean_text: &str, now: Instant) -> Option<AgentState> {
        self.last_output = now;

        if self.matches_error(clean_text) {
            return self.try_transition(AgentState::Errored, now);
        }
        if self.matches_ready(clean_text) {
            self.consecutive_errors = 0;
            return self.try_transition(AgentState::Ready, now);
        }
        // Any output in Ready/Idle → Busy (no hysteresis needed)
        if self.state == AgentState::Ready || self.state == AgentState::Idle {
            return self.apply(AgentState::Busy, now);
        }
        None
    }

    /// Called periodically to check time-based transitions.
    pub fn tick(&mut self, now: Instant) -> Option<AgentState> {
        // Check pending hysteresis
        if let Some((target, since)) = self.pending {
            let required = match target {
                AgentState::Ready => READY_HYSTERESIS,
                AgentState::Busy => BUSY_HYSTERESIS,
                _ => Duration::ZERO,
            };
            if now.duration_since(since) >= required {
                self.pending = None;
                return self.apply(target, now);
            }
        }
        // Idle detection: Ready + silence
        if self.state == AgentState::Ready && now.duration_since(self.last_output) >= IDLE_TIMEOUT {
            return self.apply(AgentState::Idle, now);
        }
        None
    }

    /// Process exit event.
    pub fn on_exit(&mut self, now: Instant) -> Option<AgentState> {
        self.apply(AgentState::Crashed, now)
    }

    /// Restart initiated.
    pub fn on_restart(&mut self, now: Instant) -> Option<AgentState> {
        self.apply(AgentState::Restarting, now)
    }

    /// Restart complete (new PTY spawned).
    pub fn on_restart_complete(&mut self, now: Instant) -> Option<AgentState> {
        self.apply(AgentState::Starting, now)
    }

    fn matches_ready(&self, text: &str) -> bool {
        let lower = text.to_lowercase();
        self.patterns.ready_patterns.iter().any(|p| lower.contains(p))
    }

    fn matches_error(&self, text: &str) -> bool {
        let lower = text.to_lowercase();
        self.patterns.error_patterns.iter().any(|p| lower.contains(p))
    }

    fn try_transition(&mut self, target: AgentState, now: Instant) -> Option<AgentState> {
        if Self::transition(self.state, &event_for(target)).is_none() {
            return None;
        }
        // States needing hysteresis
        let needs_hysteresis = matches!(target, AgentState::Ready);
        if needs_hysteresis {
            match &self.pending {
                Some((t, _)) if *t == target => None, // already pending
                _ => { self.pending = Some((target, now)); None }
            }
        } else {
            self.apply(target, now)
        }
    }

    fn apply(&mut self, target: AgentState, _now: Instant) -> Option<AgentState> {
        let ev = event_for(target);
        if Self::transition(self.state, &ev).is_some() {
            if target == AgentState::Errored {
                self.consecutive_errors += 1;
            }
            self.state = target;
            self.pending = None;
            Some(target)
        } else {
            None
        }
    }
}

/// Map a target state to the event that would cause it (for transition validation).
fn event_for(state: AgentState) -> StateEvent {
    match state {
        AgentState::Starting => StateEvent::RestartComplete,
        AgentState::Ready => StateEvent::ReadyPatternDetected,
        AgentState::Busy => StateEvent::OutputReceived,
        AgentState::Idle => StateEvent::SilenceDuration(IDLE_TIMEOUT),
        AgentState::Errored => StateEvent::ErrorPatternDetected,
        AgentState::Crashed => StateEvent::ProcessExited,
        AgentState::Restarting => StateEvent::RestartInitiated,
    }
}

/// Strip ANSI escape sequences from text (same logic as daemon.rs).
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    while let Some(&ch) = chars.peek() {
                        chars.next();
                        if ch.is_ascii_alphabetic() { break; }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn claude_patterns() -> StatePatterns {
        StatePatterns::from_backend("Type your")
    }

    fn make_sm() -> StateMachine {
        StateMachine::new(claude_patterns())
    }

    // ── Transition table tests ──────────────────────────────────────

    #[test]
    fn starting_to_ready() {
        assert_eq!(
            StateMachine::transition(AgentState::Starting, &StateEvent::ReadyPatternDetected),
            Some(AgentState::Ready)
        );
    }

    #[test]
    fn starting_to_errored() {
        assert_eq!(
            StateMachine::transition(AgentState::Starting, &StateEvent::ErrorPatternDetected),
            Some(AgentState::Errored)
        );
    }

    #[test]
    fn starting_to_crashed() {
        assert_eq!(
            StateMachine::transition(AgentState::Starting, &StateEvent::ProcessExited),
            Some(AgentState::Crashed)
        );
    }

    #[test]
    fn ready_to_busy() {
        assert_eq!(
            StateMachine::transition(AgentState::Ready, &StateEvent::OutputReceived),
            Some(AgentState::Busy)
        );
    }

    #[test]
    fn ready_to_idle() {
        assert_eq!(
            StateMachine::transition(AgentState::Ready, &StateEvent::SilenceDuration(Duration::from_secs(30))),
            Some(AgentState::Idle)
        );
    }

    #[test]
    fn busy_to_ready() {
        assert_eq!(
            StateMachine::transition(AgentState::Busy, &StateEvent::ReadyPatternDetected),
            Some(AgentState::Ready)
        );
    }

    #[test]
    fn busy_to_crashed() {
        assert_eq!(
            StateMachine::transition(AgentState::Busy, &StateEvent::ProcessExited),
            Some(AgentState::Crashed)
        );
    }

    #[test]
    fn idle_to_busy() {
        assert_eq!(
            StateMachine::transition(AgentState::Idle, &StateEvent::OutputReceived),
            Some(AgentState::Busy)
        );
    }

    #[test]
    fn crashed_to_restarting() {
        assert_eq!(
            StateMachine::transition(AgentState::Crashed, &StateEvent::RestartInitiated),
            Some(AgentState::Restarting)
        );
    }

    #[test]
    fn restarting_to_starting() {
        assert_eq!(
            StateMachine::transition(AgentState::Restarting, &StateEvent::RestartComplete),
            Some(AgentState::Starting)
        );
    }

    #[test]
    fn invalid_transitions_return_none() {
        assert_eq!(StateMachine::transition(AgentState::Starting, &StateEvent::OutputReceived), None);
        assert_eq!(StateMachine::transition(AgentState::Crashed, &StateEvent::ReadyPatternDetected), None);
        assert_eq!(StateMachine::transition(AgentState::Restarting, &StateEvent::OutputReceived), None);
        assert_eq!(StateMachine::transition(AgentState::Ready, &StateEvent::RestartInitiated), None);
    }

    // ── Pattern matching tests ──────────────────────────────────────

    #[test]
    fn ready_pattern_detected_from_output() {
        let mut sm = make_sm();
        let now = Instant::now();
        // "Type your" triggers pending Ready
        sm.process_output("Type your question", now);
        assert_eq!(sm.state(), AgentState::Starting); // still pending (hysteresis)
        // After hysteresis
        let later = now + READY_HYSTERESIS;
        let result = sm.tick(later);
        assert_eq!(result, Some(AgentState::Ready));
        assert_eq!(sm.state(), AgentState::Ready);
    }

    #[test]
    fn error_pattern_transitions_to_errored() {
        let mut sm = make_sm();
        let now = Instant::now();
        let result = sm.process_output("FATAL error occurred", now);
        assert_eq!(result, Some(AgentState::Errored));
    }

    #[test]
    fn output_in_ready_transitions_to_busy() {
        let mut sm = make_sm();
        let now = Instant::now();
        // Get to Ready state
        sm.process_output("Type your question", now);
        sm.tick(now + READY_HYSTERESIS);
        assert_eq!(sm.state(), AgentState::Ready);
        // Any non-pattern output → Busy
        let result = sm.process_output("processing something", now + Duration::from_secs(1));
        assert_eq!(result, Some(AgentState::Busy));
    }

    #[test]
    fn idle_after_silence() {
        let mut sm = make_sm();
        let now = Instant::now();
        sm.process_output("Type your question", now);
        sm.tick(now + READY_HYSTERESIS);
        assert_eq!(sm.state(), AgentState::Ready);
        // No output for IDLE_TIMEOUT
        let result = sm.tick(now + IDLE_TIMEOUT + Duration::from_secs(1));
        assert_eq!(result, Some(AgentState::Idle));
    }

    #[test]
    fn idle_to_busy_on_output() {
        let mut sm = make_sm();
        let now = Instant::now();
        sm.process_output("Type your question", now);
        sm.tick(now + READY_HYSTERESIS);
        sm.tick(now + IDLE_TIMEOUT + Duration::from_secs(1));
        assert_eq!(sm.state(), AgentState::Idle);
        let result = sm.process_output("new output", now + IDLE_TIMEOUT + Duration::from_secs(2));
        assert_eq!(result, Some(AgentState::Busy));
    }

    #[test]
    fn consecutive_errors_tracked() {
        let mut sm = make_sm();
        let now = Instant::now();
        sm.process_output("fatal crash", now);
        assert_eq!(sm.consecutive_errors(), 1);
        // Errored → Ready (reset)
        sm.process_output("Type your question", now);
        sm.tick(now + READY_HYSTERESIS);
        assert_eq!(sm.consecutive_errors(), 0);
        // Ready → Errored again
        sm.process_output("error again", now + Duration::from_secs(1));
        assert_eq!(sm.consecutive_errors(), 1);
    }

    #[test]
    fn process_exit_from_any_active_state() {
        for state in [AgentState::Starting, AgentState::Ready, AgentState::Busy, AgentState::Idle] {
            let mut sm = make_sm();
            sm.state = state;
            let result = sm.on_exit(Instant::now());
            assert_eq!(result, Some(AgentState::Crashed), "exit from {state:?}");
        }
    }

    #[test]
    fn restart_cycle() {
        let mut sm = make_sm();
        let now = Instant::now();
        sm.on_exit(now);
        assert_eq!(sm.state(), AgentState::Crashed);
        sm.on_restart(now);
        assert_eq!(sm.state(), AgentState::Restarting);
        sm.on_restart_complete(now);
        assert_eq!(sm.state(), AgentState::Starting);
    }

    // ── Multi-backend pattern tests ─────────────────────────────────

    #[test]
    fn gemini_ready_pattern() {
        let patterns = StatePatterns::from_backend(">|gemini");
        let mut sm = StateMachine::new(patterns);
        let now = Instant::now();
        sm.process_output("gemini> ", now);
        sm.tick(now + READY_HYSTERESIS);
        assert_eq!(sm.state(), AgentState::Ready);
    }

    #[test]
    fn kiro_ready_pattern() {
        let patterns = StatePatterns::from_backend("ready|chat|>");
        let mut sm = StateMachine::new(patterns);
        let now = Instant::now();
        sm.process_output("Kiro is ready", now);
        sm.tick(now + READY_HYSTERESIS);
        assert_eq!(sm.state(), AgentState::Ready);
    }

    // ── ANSI stripping ──────────────────────────────────────────────

    #[test]
    fn strip_ansi_removes_escapes() {
        assert_eq!(strip_ansi("\x1b[32mHello\x1b[0m"), "Hello");
        assert_eq!(strip_ansi("\x1b]0;title\x07text"), "text");
        assert_eq!(strip_ansi("plain text"), "plain text");
    }

    // ── Hysteresis prevents flapping ────────────────────────────────

    #[test]
    fn hysteresis_prevents_premature_ready() {
        let mut sm = make_sm();
        let now = Instant::now();
        sm.process_output("Type your question", now);
        // Not enough time passed
        let result = sm.tick(now + Duration::from_millis(100));
        assert_eq!(result, None);
        assert_eq!(sm.state(), AgentState::Starting);
    }
}

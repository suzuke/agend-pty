//! Agent lifecycle state machine — tracks agent state based on PTY output patterns.
//!
//! Design principles (from agend-terminal review):
//! - Hysteresis on ESCALATION (→Errored): sustained error before confirming (prevent false positives)
//! - IMMEDIATE on RECOVERY (→Ready): recover as fast as possible
//! - Clear detection buffer on state transition to prevent stale pattern re-triggers

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
    WaitingForInput, // permission prompts, trust dialogs, etc.
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
    InputRequested, // permission/trust prompt detected
}

/// Classifies errors for logging/notification and respawn decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    RateLimit,
    AuthError, // permanent — should NOT respawn
    ContextFull,
    ApiError,
}

impl ErrorKind {
    pub fn is_permanent(&self) -> bool {
        matches!(self, ErrorKind::AuthError)
    }

    fn detect(text: &str, state: AgentState) -> Option<ErrorKind> {
        let lower = text.to_lowercase();
        if lower.contains("rate limit") || lower.contains("429") {
            return Some(ErrorKind::RateLimit);
        }
        if lower.contains("unauthorized")
            || lower.contains("invalid api key")
            || lower.contains("401")
        {
            return Some(ErrorKind::AuthError);
        }
        if lower.contains("context")
            && (lower.contains("full") || lower.contains("limit") || lower.contains("too long"))
        {
            return Some(ErrorKind::ContextFull);
        }
        // Starting state: broad patterns (catching startup errors)
        if matches!(state, AgentState::Starting) {
            if lower.contains("error") || lower.contains("fatal") || lower.contains("panic") {
                return Some(ErrorKind::ApiError);
            }
        } else {
            // Ready/Busy/Idle: precise patterns only
            if lower.contains("error:")
                || lower.contains("fatal:")
                || lower.contains("panic:")
                || lower.contains("FATAL")
                || lower.contains("thread") && lower.contains("panicked")
            {
                return Some(ErrorKind::ApiError);
            }
        }
        None
    }
}

/// Per-backend patterns for state detection.
#[derive(Clone)]
pub struct StatePatterns {
    pub ready_patterns: Vec<String>,
    pub input_patterns: Vec<String>,
}

impl StatePatterns {
    pub fn from_backend(ready_pattern: &str) -> Self {
        Self {
            ready_patterns: ready_pattern.split('|').map(|s| s.to_lowercase()).collect(),
            input_patterns: vec![
                "yes, i trust".into(),
                "yes, proceed".into(),
                "allow once".into(),
                "allow always".into(),
                "permission required".into(),
                "grant permission".into(),
                "(y/n)".into(),
                "[y/n]".into(),
            ],
        }
    }
}

/// Hysteresis — only on ESCALATION to severe states (prevent false positives).
const ERROR_HYSTERESIS: Duration = Duration::from_secs(2);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Tracks agent state with directional hysteresis.
pub struct StateMachine {
    state: AgentState,
    patterns: StatePatterns,
    pending: Option<(AgentState, Instant)>,
    last_output: Instant,
    consecutive_errors: u32,
    last_error_kind: Option<ErrorKind>,
    detect_buf: String,
}

fn matches_any(patterns: &[String], text: &str) -> bool {
    let lower = text.to_lowercase();
    patterns.iter().any(|p| lower.contains(p))
}

impl StateMachine {
    pub fn new(patterns: StatePatterns) -> Self {
        Self {
            state: AgentState::Starting,
            patterns,
            pending: None,
            last_output: Instant::now(),
            consecutive_errors: 0,
            last_error_kind: None,
            detect_buf: String::new(),
        }
    }

    pub fn state(&self) -> AgentState {
        self.state
    }
    pub fn consecutive_errors(&self) -> u32 {
        self.consecutive_errors
    }
    pub fn last_error_kind(&self) -> Option<ErrorKind> {
        self.last_error_kind
    }

    /// Pure transition logic.
    pub fn transition(current: AgentState, event: &StateEvent) -> Option<AgentState> {
        use AgentState::*;
        use StateEvent::*;
        match (current, event) {
            (Starting, ReadyPatternDetected) => Some(Ready),
            (Starting, ErrorPatternDetected) => Some(Errored),
            (Starting, ProcessExited) => Some(Crashed),
            (Starting, InputRequested) => Some(WaitingForInput),

            (Ready, OutputReceived) => Some(Busy),
            (Ready, SilenceDuration(_)) => Some(Idle),
            (Ready, ErrorPatternDetected) => Some(Errored),
            (Ready, ProcessExited) => Some(Crashed),

            (Busy, ReadyPatternDetected) => Some(Ready),
            (Busy, ErrorPatternDetected) => Some(Errored),
            (Busy, ProcessExited) => Some(Crashed),
            (Busy, InputRequested) => Some(WaitingForInput),

            (Idle, OutputReceived) => Some(Busy),
            (Idle, ErrorPatternDetected) => Some(Errored),
            (Idle, ProcessExited) => Some(Crashed),

            (Errored, ReadyPatternDetected) => Some(Ready),
            (Errored, ProcessExited) => Some(Crashed),
            (Errored, RestartInitiated) => Some(Restarting),

            (Crashed, RestartInitiated) => Some(Restarting),

            (Restarting, RestartComplete) => Some(Starting),

            (WaitingForInput, ReadyPatternDetected) => Some(Ready),
            (WaitingForInput, OutputReceived) => Some(Busy),
            (WaitingForInput, ProcessExited) => Some(Crashed),

            _ => None,
        }
    }

    /// Feed stripped PTY output. Returns new state if changed.
    pub fn process_output(&mut self, clean_text: &str, now: Instant) -> Option<AgentState> {
        self.last_output = now;
        self.detect_buf.push_str(clean_text);
        if self.detect_buf.len() > 4096 {
            let keep_from = self
                .detect_buf
                .ceil_char_boundary(self.detect_buf.len() - 4096);
            self.detect_buf = self.detect_buf[keep_from..].to_string();
        }

        let buf = self.detect_buf.clone();
        if matches_any(&self.patterns.input_patterns, &buf) {
            if let Some(s) = self.try_transition_directed(AgentState::WaitingForInput, now) {
                return Some(s);
            }
        }
        if let Some(kind) = ErrorKind::detect(&buf, self.state) {
            self.last_error_kind = Some(kind);
            return self.try_transition_directed(AgentState::Errored, now);
        }
        if matches_any(&self.patterns.ready_patterns, &buf) {
            self.consecutive_errors = 0;
            return self.try_transition_directed(AgentState::Ready, now);
        }
        if self.state == AgentState::Ready || self.state == AgentState::Idle {
            return self.apply(AgentState::Busy, now);
        }
        None
    }

    /// Periodic tick for time-based transitions.
    pub fn tick(&mut self, now: Instant) -> Option<AgentState> {
        // Check pending hysteresis (error escalation)
        if let Some((target, since)) = self.pending {
            let required = match target {
                AgentState::Errored => ERROR_HYSTERESIS,
                _ => Duration::ZERO,
            };
            if now.duration_since(since) >= required {
                self.pending = None;
                return self.apply(target, now);
            }
        }
        // Idle detection
        if self.state == AgentState::Ready && now.duration_since(self.last_output) >= IDLE_TIMEOUT {
            return self.apply(AgentState::Idle, now);
        }
        None
    }

    pub fn on_exit(&mut self, now: Instant) -> Option<AgentState> {
        self.apply(AgentState::Crashed, now)
    }

    pub fn on_restart(&mut self, now: Instant) -> Option<AgentState> {
        self.apply(AgentState::Restarting, now)
    }

    pub fn on_restart_complete(&mut self, now: Instant) -> Option<AgentState> {
        self.apply(AgentState::Starting, now)
    }

    /// Directional hysteresis: escalation (→Errored) = debounce, recovery (→Ready) = immediate.
    fn try_transition_directed(&mut self, target: AgentState, now: Instant) -> Option<AgentState> {
        Self::transition(self.state, &event_for(target))?;
        let is_escalation = matches!(target, AgentState::Errored);
        if is_escalation {
            match &self.pending {
                Some((t, _)) if *t == target => None,
                _ => {
                    self.pending = Some((target, now));
                    None
                }
            }
        } else {
            self.apply(target, now)
        }
    }

    fn apply(&mut self, target: AgentState, _now: Instant) -> Option<AgentState> {
        if Self::transition(self.state, &event_for(target)).is_some() {
            if target == AgentState::Errored {
                self.consecutive_errors += 1;
            }
            self.state = target;
            self.pending = None;
            self.detect_buf.clear(); // Finding #2: clear buffer on transition
            Some(target)
        } else {
            None
        }
    }
}

fn event_for(state: AgentState) -> StateEvent {
    match state {
        AgentState::Starting => StateEvent::RestartComplete,
        AgentState::Ready => StateEvent::ReadyPatternDetected,
        AgentState::Busy => StateEvent::OutputReceived,
        AgentState::Idle => StateEvent::SilenceDuration(IDLE_TIMEOUT),
        AgentState::Errored => StateEvent::ErrorPatternDetected,
        AgentState::Crashed => StateEvent::ProcessExited,
        AgentState::Restarting => StateEvent::RestartInitiated,
        AgentState::WaitingForInput => StateEvent::InputRequested,
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

    fn claude_patterns() -> StatePatterns {
        StatePatterns::from_backend("Type your")
    }
    fn make_sm() -> StateMachine {
        StateMachine::new(claude_patterns())
    }

    // ── Transition table ────────────────────────────────────────────

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
    fn starting_to_waiting() {
        assert_eq!(
            StateMachine::transition(AgentState::Starting, &StateEvent::InputRequested),
            Some(AgentState::WaitingForInput)
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
            StateMachine::transition(
                AgentState::Ready,
                &StateEvent::SilenceDuration(Duration::from_secs(30))
            ),
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
    fn busy_to_waiting() {
        assert_eq!(
            StateMachine::transition(AgentState::Busy, &StateEvent::InputRequested),
            Some(AgentState::WaitingForInput)
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
    fn waiting_to_ready() {
        assert_eq!(
            StateMachine::transition(
                AgentState::WaitingForInput,
                &StateEvent::ReadyPatternDetected
            ),
            Some(AgentState::Ready)
        );
    }
    #[test]
    fn waiting_to_busy() {
        assert_eq!(
            StateMachine::transition(AgentState::WaitingForInput, &StateEvent::OutputReceived),
            Some(AgentState::Busy)
        );
    }
    #[test]
    fn invalid_transitions() {
        assert_eq!(
            StateMachine::transition(AgentState::Starting, &StateEvent::OutputReceived),
            None
        );
        assert_eq!(
            StateMachine::transition(AgentState::Crashed, &StateEvent::ReadyPatternDetected),
            None
        );
        assert_eq!(
            StateMachine::transition(AgentState::Restarting, &StateEvent::OutputReceived),
            None
        );
    }

    // ── Finding #1: Hysteresis on ESCALATION, immediate RECOVERY ────

    #[test]
    fn error_has_hysteresis() {
        let mut sm = make_sm();
        let now = Instant::now();
        // Error detected but not yet confirmed
        sm.process_output("fatal error occurred", now);
        assert_eq!(sm.state(), AgentState::Starting); // still pending
                                                      // After hysteresis period → confirmed
        let result = sm.tick(now + ERROR_HYSTERESIS);
        assert_eq!(result, Some(AgentState::Errored));
    }

    #[test]
    fn ready_is_immediate_no_hysteresis() {
        let mut sm = make_sm();
        let now = Instant::now();
        // Ready pattern → immediate transition (no tick needed)
        let result = sm.process_output("Type your question", now);
        assert_eq!(result, Some(AgentState::Ready));
        assert_eq!(sm.state(), AgentState::Ready);
    }

    #[test]
    fn recovery_from_errored_is_immediate() {
        let mut sm = make_sm();
        let now = Instant::now();
        sm.state = AgentState::Errored;
        sm.consecutive_errors = 1;
        let result = sm.process_output("Type your question", now);
        assert_eq!(result, Some(AgentState::Ready));
    }

    // ── Finding #2: Buffer cleared on transition ────────────────────

    #[test]
    fn buffer_cleared_on_transition() {
        let mut sm = make_sm();
        let now = Instant::now();
        sm.process_output("Type your question", now);
        assert_eq!(sm.state(), AgentState::Ready);
        assert!(
            sm.detect_buf.is_empty(),
            "buffer should be cleared after transition"
        );
    }

    #[test]
    fn stale_pattern_does_not_retrigger() {
        let mut sm = make_sm();
        let now = Instant::now();
        sm.process_output("Type your question", now);
        assert_eq!(sm.state(), AgentState::Ready);
        // Feed non-matching output → Busy
        sm.process_output("working on it", now + Duration::from_secs(1));
        assert_eq!(sm.state(), AgentState::Busy);
        // Old "Type your" is gone from buffer, new output doesn't contain it
        // Feed more non-matching → stays Busy (no false Ready)
        sm.process_output("still working", now + Duration::from_secs(2));
        assert_eq!(sm.state(), AgentState::Busy);
    }

    // ── Finding #3: WaitingForInput ─────────────────────────────────

    #[test]
    fn trust_dialog_triggers_waiting() {
        let mut sm = make_sm();
        let now = Instant::now();
        let result = sm.process_output("Do you trust this? Yes, I trust", now);
        assert_eq!(result, Some(AgentState::WaitingForInput));
    }

    #[test]
    fn waiting_recovers_to_ready() {
        let mut sm = make_sm();
        let now = Instant::now();
        sm.process_output("Yes, I trust", now);
        assert_eq!(sm.state(), AgentState::WaitingForInput);
        let result = sm.process_output("Type your question", now + Duration::from_secs(1));
        assert_eq!(result, Some(AgentState::Ready));
    }

    // ── Finding #4: ErrorKind ───────────────────────────────────────

    #[test]
    fn error_kind_detection() {
        assert_eq!(
            ErrorKind::detect("rate limit exceeded", AgentState::Starting),
            Some(ErrorKind::RateLimit)
        );
        assert_eq!(
            ErrorKind::detect("HTTP 429 too many", AgentState::Starting),
            Some(ErrorKind::RateLimit)
        );
        assert_eq!(
            ErrorKind::detect("unauthorized access", AgentState::Starting),
            Some(ErrorKind::AuthError)
        );
        assert_eq!(
            ErrorKind::detect("invalid api key", AgentState::Starting),
            Some(ErrorKind::AuthError)
        );
        assert_eq!(
            ErrorKind::detect("context too long", AgentState::Starting),
            Some(ErrorKind::ContextFull)
        );
        assert_eq!(
            ErrorKind::detect("fatal crash", AgentState::Starting),
            Some(ErrorKind::ApiError)
        );
        // Precise patterns in non-Starting states
        assert_eq!(
            ErrorKind::detect("error: something", AgentState::Ready),
            Some(ErrorKind::ApiError)
        );
        assert_eq!(
            ErrorKind::detect("some error happened", AgentState::Ready),
            None
        ); // "error" without colon
        assert_eq!(ErrorKind::detect("all good", AgentState::Starting), None);
    }

    #[test]
    fn auth_error_is_permanent() {
        assert!(ErrorKind::AuthError.is_permanent());
        assert!(!ErrorKind::RateLimit.is_permanent());
        assert!(!ErrorKind::ApiError.is_permanent());
    }

    #[test]
    fn error_kind_stored_on_error() {
        let mut sm = make_sm();
        let now = Instant::now();
        sm.process_output("unauthorized access denied", now);
        sm.tick(now + ERROR_HYSTERESIS);
        assert_eq!(sm.last_error_kind(), Some(ErrorKind::AuthError));
    }

    // ── Existing behavior preserved ─────────────────────────────────

    #[test]
    fn output_in_ready_transitions_to_busy() {
        let mut sm = make_sm();
        let now = Instant::now();
        sm.process_output("Type your question", now);
        assert_eq!(sm.state(), AgentState::Ready);
        let result = sm.process_output("processing something", now + Duration::from_secs(1));
        assert_eq!(result, Some(AgentState::Busy));
    }

    #[test]
    fn idle_after_silence() {
        let mut sm = make_sm();
        let now = Instant::now();
        sm.process_output("Type your question", now);
        let result = sm.tick(now + IDLE_TIMEOUT + Duration::from_secs(1));
        assert_eq!(result, Some(AgentState::Idle));
    }

    #[test]
    fn consecutive_errors_tracked() {
        let mut sm = make_sm();
        let now = Instant::now();
        sm.process_output("fatal crash", now);
        sm.tick(now + ERROR_HYSTERESIS);
        assert_eq!(sm.consecutive_errors(), 1);
        sm.process_output("Type your question", now + Duration::from_secs(3));
        assert_eq!(sm.consecutive_errors(), 0);
    }

    #[test]
    fn process_exit_from_any_active_state() {
        for state in [
            AgentState::Starting,
            AgentState::Ready,
            AgentState::Busy,
            AgentState::Idle,
            AgentState::WaitingForInput,
        ] {
            let mut sm = make_sm();
            sm.state = state;
            assert_eq!(
                sm.on_exit(Instant::now()),
                Some(AgentState::Crashed),
                "exit from {state:?}"
            );
        }
    }

    #[test]
    fn restart_cycle() {
        let mut sm = make_sm();
        let now = Instant::now();
        sm.on_exit(now);
        sm.on_restart(now);
        sm.on_restart_complete(now);
        assert_eq!(sm.state(), AgentState::Starting);
    }

    #[test]
    fn gemini_ready_pattern() {
        let mut sm = StateMachine::new(StatePatterns::from_backend(">|gemini"));
        let now = Instant::now();
        let result = sm.process_output("gemini> ", now);
        assert_eq!(result, Some(AgentState::Ready));
    }

    #[test]
    fn strip_ansi_removes_escapes() {
        assert_eq!(strip_ansi("\x1b[32mHello\x1b[0m"), "Hello");
        assert_eq!(strip_ansi("\x1b]0;title\x07text"), "text");
    }
}

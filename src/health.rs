//! Health monitoring — auto-respawn, backoff, crash detection, hang detection.
//!
//! Design principles:
//! - Backoff based on sliding window crash count, NOT total_crashes
//! - AuthError (permanent) blocks auto-respawn
//! - Window expiry naturally resets backoff — no manual reset needed
//! - Error states derived from AgentState itself (no separate ErrorKind)

use crate::state::AgentState;
use std::time::{Duration, Instant};

const INITIAL_BACKOFF: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(300);
const CRASH_WINDOW: Duration = Duration::from_secs(600); // 10 minutes
const MAX_CRASHES_IN_WINDOW: u32 = 3;
const HANG_TIMEOUT: Duration = Duration::from_secs(900); // 15 minutes
const MAX_CONSECUTIVE_ERRORS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    Healthy,
    Degraded,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthAction {
    None,
    Restart,
    MarkFailed,
    KillAndRestart,
}

pub struct HealthMonitor {
    status: HealthStatus,
    crash_times: Vec<Instant>,
    last_restart: Option<Instant>,
    busy_since: Option<Instant>,
    session_start: Instant,
    max_session_secs: Option<u64>,
    session_warned: bool,
}

impl Default for HealthMonitor {
    fn default() -> Self {
        Self {
            status: HealthStatus::Healthy,
            crash_times: Vec::new(),
            last_restart: None,
            busy_since: None,
            session_start: Instant::now(),
            max_session_secs: None,
            session_warned: false,
        }
    }
}

impl HealthMonitor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_max_session_hours(&mut self, hours: f64) {
        if hours > 0.0 {
            self.max_session_secs = Some((hours * 3600.0) as u64);
        }
    }

    pub fn status(&self) -> HealthStatus {
        self.status
    }

    /// Backoff from sliding window crash count.
    pub fn backoff_duration(&self, now: Instant) -> Duration {
        let window_crashes = self.crashes_in_window(now);
        if window_crashes <= 1 {
            return INITIAL_BACKOFF;
        }
        let secs = INITIAL_BACKOFF.as_secs() * (1u64 << (window_crashes - 1).min(6));
        Duration::from_secs(secs.min(MAX_BACKOFF.as_secs()))
    }

    /// Called when agent state changes. Error kind is derived from state itself.
    pub fn on_state_change(
        &mut self,
        state: AgentState,
        consecutive_errors: u32,
        now: Instant,
    ) -> HealthAction {
        // Unavailable states
        if state == AgentState::Crashed {
            return self.on_crash(now);
        }

        // Working states (active, legit busy)
        if state.is_working() {
            self.busy_since = Some(now);
            return HealthAction::None;
        }

        // Hang — always kill and restart
        if state == AgentState::Hang {
            self.busy_since = None;
            return HealthAction::KillAndRestart;
        }

        // Passive / waiting — reset busy tracker, recover from Degraded
        if state.is_passive() || state.is_waiting_input() {
            self.busy_since = None;
            if self.status == HealthStatus::Degraded {
                self.status = HealthStatus::Healthy;
            }
            return HealthAction::None;
        }

        // Error states
        if state.is_error() {
            if state.is_permanent_error() {
                self.status = HealthStatus::Failed;
                return HealthAction::MarkFailed;
            }
            if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                self.status = HealthStatus::Failed;
                return HealthAction::MarkFailed;
            }
            return HealthAction::None;
        }

        // Starting / Restarting — no action
        HealthAction::None
    }

    /// Periodic check.
    pub fn tick(&mut self, current_state: AgentState, now: Instant) -> HealthAction {
        if self.status == HealthStatus::Failed {
            return HealthAction::None;
        }

        // Hang detection from prolonged working state
        if current_state.is_working() {
            if let Some(since) = self.busy_since {
                if now.duration_since(since) >= HANG_TIMEOUT {
                    self.busy_since = None;
                    return HealthAction::KillAndRestart;
                }
            }
        }

        // Backoff-gated restart
        if current_state == AgentState::Crashed && self.status == HealthStatus::Degraded {
            if let Some(last) = self.last_restart {
                if now.duration_since(last) >= self.backoff_duration(now) {
                    return HealthAction::Restart;
                }
            }
        }

        // Natural recovery — if window has no crashes, restore healthy
        if self.status == HealthStatus::Degraded && self.crashes_in_window(now) == 0 {
            self.status = HealthStatus::Healthy;
        }

        // Session timer: warn at 80%, mark failed at 100%
        if let Some(max) = self.max_session_secs {
            let elapsed = now.duration_since(self.session_start).as_secs();
            if elapsed >= max {
                self.status = HealthStatus::Failed;
                return HealthAction::MarkFailed;
            }
            if !self.session_warned && elapsed >= max * 4 / 5 {
                self.session_warned = true;
            }
        }

        HealthAction::None
    }

    /// Returns true once when session reaches 80% of max (for caller to warn).
    pub fn check_session_warning(&self, now: Instant) -> bool {
        if let Some(max) = self.max_session_secs {
            let elapsed = now.duration_since(self.session_start).as_secs();
            self.session_warned && elapsed >= max * 4 / 5 && elapsed < max
        } else {
            false
        }
    }

    pub fn on_restart(&mut self, now: Instant) {
        self.last_restart = Some(now);
        self.busy_since = None;
    }

    pub fn reset(&mut self) {
        self.status = HealthStatus::Healthy;
        self.crash_times.clear();
        self.last_restart = None;
        self.busy_since = None;
        self.session_start = Instant::now();
        self.session_warned = false;
    }

    fn crashes_in_window(&self, now: Instant) -> u32 {
        self.crash_times
            .iter()
            .filter(|t| now.duration_since(**t) < CRASH_WINDOW)
            .count() as u32
    }

    fn on_crash(&mut self, now: Instant) -> HealthAction {
        self.crash_times.push(now);
        // Prune very old entries (>2x window) to prevent unbounded growth
        let cutoff = CRASH_WINDOW + CRASH_WINDOW;
        self.crash_times.retain(|t| now.duration_since(*t) < cutoff);

        if self.crashes_in_window(now) >= MAX_CRASHES_IN_WINDOW {
            self.status = HealthStatus::Failed;
            return HealthAction::MarkFailed;
        }

        self.status = HealthStatus::Degraded;
        if self.last_restart.is_none() {
            self.last_restart = Some(now);
        }
        HealthAction::Restart
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state() {
        let hm = HealthMonitor::new();
        assert_eq!(hm.status(), HealthStatus::Healthy);
    }

    // ── Sliding window backoff ──────────────────────────────────────

    #[test]
    fn backoff_from_window_crashes() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.crash_times.push(now);
        assert_eq!(hm.backoff_duration(now), Duration::from_secs(5));
        hm.crash_times.push(now + Duration::from_secs(1));
        assert_eq!(
            hm.backoff_duration(now + Duration::from_secs(1)),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn backoff_resets_when_window_expires() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.crash_times.push(now);
        hm.crash_times.push(now + Duration::from_secs(1));
        let later = now + CRASH_WINDOW + Duration::from_secs(1);
        assert_eq!(hm.backoff_duration(later), Duration::from_secs(5));
    }

    #[test]
    fn backoff_capped() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        for i in 0..10 {
            hm.crash_times.push(now + Duration::from_secs(i));
        }
        assert!(hm.backoff_duration(now + Duration::from_secs(10)) <= MAX_BACKOFF);
    }

    // ── Crash window ────────────────────────────────────────────────

    #[test]
    fn single_crash_triggers_restart() {
        let mut hm = HealthMonitor::new();
        let action = hm.on_state_change(AgentState::Crashed, 0, Instant::now());
        assert_eq!(action, HealthAction::Restart);
        assert_eq!(hm.status(), HealthStatus::Degraded);
    }

    #[test]
    fn three_crashes_in_window_marks_failed() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.on_state_change(AgentState::Crashed, 0, now);
        hm.on_state_change(AgentState::Crashed, 0, now + Duration::from_secs(60));
        let action = hm.on_state_change(AgentState::Crashed, 0, now + Duration::from_secs(120));
        assert_eq!(action, HealthAction::MarkFailed);
    }

    #[test]
    fn old_crashes_outside_window_dont_count() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.on_state_change(AgentState::Crashed, 0, now);
        hm.on_state_change(AgentState::Crashed, 0, now + Duration::from_secs(60));
        let action = hm.on_state_change(AgentState::Crashed, 0, now + Duration::from_secs(700));
        assert_eq!(action, HealthAction::Restart);
    }

    // ── Natural recovery via window expiry ──────────────────────────

    #[test]
    fn degraded_recovers_when_window_clears() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.on_state_change(AgentState::Crashed, 0, now);
        assert_eq!(hm.status(), HealthStatus::Degraded);
        hm.tick(
            AgentState::Starting,
            now + CRASH_WINDOW + Duration::from_secs(1),
        );
        assert_eq!(hm.status(), HealthStatus::Healthy);
    }

    // ── AuthError (permanent) blocks respawn ────────────────────────

    #[test]
    fn auth_error_marks_failed() {
        let mut hm = HealthMonitor::new();
        let action = hm.on_state_change(AgentState::AuthError, 1, Instant::now());
        assert_eq!(action, HealthAction::MarkFailed);
        assert_eq!(hm.status(), HealthStatus::Failed);
    }

    #[test]
    fn rate_limit_does_not_mark_failed() {
        let mut hm = HealthMonitor::new();
        let action = hm.on_state_change(AgentState::RateLimit, 1, Instant::now());
        assert_eq!(action, HealthAction::None);
    }

    #[test]
    fn consecutive_errors_without_permanent_marks_failed() {
        let mut hm = HealthMonitor::new();
        let action = hm.on_state_change(AgentState::ApiError, 3, Instant::now());
        assert_eq!(action, HealthAction::MarkFailed);
    }

    #[test]
    fn context_full_not_permanent() {
        let mut hm = HealthMonitor::new();
        let action = hm.on_state_change(AgentState::ContextFull, 1, Instant::now());
        assert_eq!(action, HealthAction::None);
    }

    #[test]
    fn usage_limit_not_permanent() {
        let mut hm = HealthMonitor::new();
        let action = hm.on_state_change(AgentState::UsageLimit, 1, Instant::now());
        assert_eq!(action, HealthAction::None);
    }

    // ── Hang detection ──────────────────────────────────────────────

    #[test]
    fn hang_state_immediate_kill() {
        let mut hm = HealthMonitor::new();
        let action = hm.on_state_change(AgentState::Hang, 0, Instant::now());
        assert_eq!(action, HealthAction::KillAndRestart);
    }

    #[test]
    fn working_busy_since_tracked() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.on_state_change(AgentState::Thinking, 0, now);
        assert_eq!(
            hm.tick(AgentState::Thinking, now + Duration::from_secs(600)),
            HealthAction::None
        );
        assert_eq!(
            hm.tick(AgentState::Thinking, now + HANG_TIMEOUT),
            HealthAction::KillAndRestart
        );
    }

    #[test]
    fn tool_use_also_tracks_hang() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.on_state_change(AgentState::ToolUse, 0, now);
        assert_eq!(
            hm.tick(AgentState::ToolUse, now + HANG_TIMEOUT),
            HealthAction::KillAndRestart
        );
    }

    #[test]
    fn busy_reset_on_ready() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.on_state_change(AgentState::Thinking, 0, now);
        hm.on_state_change(AgentState::Ready, 0, now + Duration::from_secs(60));
        assert_eq!(
            hm.tick(AgentState::Ready, now + HANG_TIMEOUT),
            HealthAction::None
        );
    }

    #[test]
    fn permission_prompt_resets_busy_since() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.on_state_change(AgentState::Thinking, 0, now);
        hm.on_state_change(AgentState::PermissionPrompt, 0, now + Duration::from_secs(10));
        assert_eq!(
            hm.tick(AgentState::PermissionPrompt, now + HANG_TIMEOUT),
            HealthAction::None
        );
    }

    // ── Recovery ────────────────────────────────────────────────────

    #[test]
    fn ready_after_degraded_restores_healthy() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.on_state_change(AgentState::Crashed, 0, now);
        hm.on_state_change(AgentState::Ready, 0, now + Duration::from_secs(10));
        assert_eq!(hm.status(), HealthStatus::Healthy);
    }

    #[test]
    fn idle_after_degraded_restores_healthy() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.on_state_change(AgentState::Crashed, 0, now);
        hm.on_state_change(AgentState::Idle, 0, now + Duration::from_secs(10));
        assert_eq!(hm.status(), HealthStatus::Healthy);
    }

    #[test]
    fn failed_no_tick_actions() {
        let mut hm = HealthMonitor::new();
        hm.status = HealthStatus::Failed;
        assert_eq!(
            hm.tick(AgentState::Crashed, Instant::now()),
            HealthAction::None
        );
    }

    #[test]
    fn tick_restart_after_backoff() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.on_state_change(AgentState::Crashed, 0, now);
        hm.on_restart(now);
        assert_eq!(
            hm.tick(AgentState::Crashed, now + Duration::from_secs(3)),
            HealthAction::None
        );
        assert_eq!(
            hm.tick(AgentState::Crashed, now + Duration::from_secs(6)),
            HealthAction::Restart
        );
    }
}

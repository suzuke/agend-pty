//! Health monitoring — auto-respawn, backoff, crash detection, hang detection.
//!
//! Design principles (from agend-terminal review):
//! - Backoff based on sliding window crash count, NOT total_crashes
//! - AuthError (permanent) blocks auto-respawn
//! - Window expiry naturally resets backoff — no manual reset needed

use crate::state::{AgentState, ErrorKind};
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
}

impl Default for HealthMonitor {
    fn default() -> Self { Self { status: HealthStatus::Healthy, crash_times: Vec::new(), last_restart: None, busy_since: None } }
}

impl HealthMonitor {
    pub fn new() -> Self { Self::default() }

    pub fn status(&self) -> HealthStatus { self.status }

    /// Backoff from sliding window crash count (Finding #5).
    /// Uses crashes_in_window instead of total restart_count.
    pub fn backoff_duration(&self, now: Instant) -> Duration {
        let window_crashes = self.crashes_in_window(now);
        if window_crashes <= 1 { return INITIAL_BACKOFF; }
        let secs = INITIAL_BACKOFF.as_secs() * (1u64 << (window_crashes - 1).min(6));
        Duration::from_secs(secs.min(MAX_BACKOFF.as_secs()))
    }

    /// Called when agent state changes.
    pub fn on_state_change(&mut self, state: AgentState, consecutive_errors: u32, error_kind: Option<ErrorKind>, now: Instant) -> HealthAction {
        match state {
            AgentState::Crashed => self.on_crash(now),
            AgentState::Busy => { self.busy_since = Some(now); HealthAction::None }
            AgentState::Ready | AgentState::Idle => {
                self.busy_since = None;
                if self.status == HealthStatus::Degraded {
                    self.status = HealthStatus::Healthy;
                }
                HealthAction::None
            }
            AgentState::Errored => {
                // Finding #4: AuthError = permanent, block respawn
                if error_kind.map(|k| k.is_permanent()).unwrap_or(false) {
                    self.status = HealthStatus::Failed;
                    return HealthAction::MarkFailed;
                }
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    self.status = HealthStatus::Failed;
                    HealthAction::MarkFailed
                } else {
                    HealthAction::None
                }
            }
            _ => HealthAction::None,
        }
    }

    /// Periodic check.
    pub fn tick(&mut self, current_state: AgentState, now: Instant) -> HealthAction {
        if self.status == HealthStatus::Failed { return HealthAction::None; }

        // Hang detection
        if current_state == AgentState::Busy {
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

        // Finding #5: Natural recovery — if window has no crashes, restore healthy
        if self.status == HealthStatus::Degraded && self.crashes_in_window(now) == 0 {
            self.status = HealthStatus::Healthy;
        }

        HealthAction::None
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
    }

    fn crashes_in_window(&self, now: Instant) -> u32 {
        self.crash_times.iter().filter(|t| now.duration_since(**t) < CRASH_WINDOW).count() as u32
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

    // ── Finding #5: Sliding window backoff ──────────────────────────

    #[test]
    fn backoff_from_window_crashes() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        // 1 crash in window → initial backoff
        hm.crash_times.push(now);
        assert_eq!(hm.backoff_duration(now), Duration::from_secs(5));
        // 2 crashes → 10s
        hm.crash_times.push(now + Duration::from_secs(1));
        assert_eq!(hm.backoff_duration(now + Duration::from_secs(1)), Duration::from_secs(10));
    }

    #[test]
    fn backoff_resets_when_window_expires() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.crash_times.push(now);
        hm.crash_times.push(now + Duration::from_secs(1));
        // After window expires, only 0 crashes in window → initial backoff
        let later = now + CRASH_WINDOW + Duration::from_secs(1);
        assert_eq!(hm.backoff_duration(later), Duration::from_secs(5));
    }

    #[test]
    fn backoff_capped() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        // Simulate many crashes (won't happen in practice due to MAX_CRASHES_IN_WINDOW)
        for i in 0..10 { hm.crash_times.push(now + Duration::from_secs(i)); }
        assert!(hm.backoff_duration(now + Duration::from_secs(10)) <= MAX_BACKOFF);
    }

    // ── Crash window ────────────────────────────────────────────────

    #[test]
    fn single_crash_triggers_restart() {
        let mut hm = HealthMonitor::new();
        let action = hm.on_state_change(AgentState::Crashed, 0, None, Instant::now());
        assert_eq!(action, HealthAction::Restart);
        assert_eq!(hm.status(), HealthStatus::Degraded);
    }

    #[test]
    fn three_crashes_in_window_marks_failed() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.on_state_change(AgentState::Crashed, 0, None, now);
        hm.on_state_change(AgentState::Crashed, 0, None, now + Duration::from_secs(60));
        let action = hm.on_state_change(AgentState::Crashed, 0, None, now + Duration::from_secs(120));
        assert_eq!(action, HealthAction::MarkFailed);
    }

    #[test]
    fn old_crashes_outside_window_dont_count() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.on_state_change(AgentState::Crashed, 0, None, now);
        hm.on_state_change(AgentState::Crashed, 0, None, now + Duration::from_secs(60));
        // Third crash after window → only 1 in window
        let action = hm.on_state_change(AgentState::Crashed, 0, None, now + Duration::from_secs(700));
        assert_eq!(action, HealthAction::Restart);
    }

    // ── Finding #5: Natural recovery via window expiry ──────────────

    #[test]
    fn degraded_recovers_when_window_clears() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.on_state_change(AgentState::Crashed, 0, None, now);
        assert_eq!(hm.status(), HealthStatus::Degraded);
        // After window expires, tick should restore healthy
        hm.tick(AgentState::Starting, now + CRASH_WINDOW + Duration::from_secs(1));
        assert_eq!(hm.status(), HealthStatus::Healthy);
    }

    // ── Finding #4: AuthError blocks respawn ────────────────────────

    #[test]
    fn auth_error_marks_failed() {
        let mut hm = HealthMonitor::new();
        let action = hm.on_state_change(AgentState::Errored, 1, Some(ErrorKind::AuthError), Instant::now());
        assert_eq!(action, HealthAction::MarkFailed);
        assert_eq!(hm.status(), HealthStatus::Failed);
    }

    #[test]
    fn rate_limit_does_not_mark_failed() {
        let mut hm = HealthMonitor::new();
        let action = hm.on_state_change(AgentState::Errored, 1, Some(ErrorKind::RateLimit), Instant::now());
        assert_eq!(action, HealthAction::None);
    }

    #[test]
    fn consecutive_errors_without_permanent_marks_failed() {
        let mut hm = HealthMonitor::new();
        let action = hm.on_state_change(AgentState::Errored, 3, Some(ErrorKind::ApiError), Instant::now());
        assert_eq!(action, HealthAction::MarkFailed);
    }

    // ── Hang detection ──────────────────────────────────────────────

    #[test]
    fn hang_detected() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.on_state_change(AgentState::Busy, 0, None, now);
        assert_eq!(hm.tick(AgentState::Busy, now + Duration::from_secs(600)), HealthAction::None);
        assert_eq!(hm.tick(AgentState::Busy, now + HANG_TIMEOUT), HealthAction::KillAndRestart);
    }

    #[test]
    fn busy_reset_on_ready() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.on_state_change(AgentState::Busy, 0, None, now);
        hm.on_state_change(AgentState::Ready, 0, None, now + Duration::from_secs(60));
        assert_eq!(hm.tick(AgentState::Ready, now + HANG_TIMEOUT), HealthAction::None);
    }

    // ── Recovery ────────────────────────────────────────────────────

    #[test]
    fn ready_after_degraded_restores_healthy() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.on_state_change(AgentState::Crashed, 0, None, now);
        hm.on_state_change(AgentState::Ready, 0, None, now + Duration::from_secs(10));
        assert_eq!(hm.status(), HealthStatus::Healthy);
    }

    #[test]
    fn failed_no_tick_actions() {
        let mut hm = HealthMonitor::new();
        hm.status = HealthStatus::Failed;
        assert_eq!(hm.tick(AgentState::Crashed, Instant::now()), HealthAction::None);
    }

    #[test]
    fn tick_restart_after_backoff() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.on_state_change(AgentState::Crashed, 0, None, now);
        hm.on_restart(now);
        assert_eq!(hm.tick(AgentState::Crashed, now + Duration::from_secs(3)), HealthAction::None);
        assert_eq!(hm.tick(AgentState::Crashed, now + Duration::from_secs(6)), HealthAction::Restart);
    }
}

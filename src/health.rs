//! Health monitoring — auto-respawn, backoff, crash detection, hang detection.
//!
//! Integrates with StateMachine to provide automatic recovery for crashed/hung agents.

use crate::state::AgentState;
use std::time::{Duration, Instant};

const INITIAL_BACKOFF: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(300);
const CRASH_WINDOW: Duration = Duration::from_secs(600); // 10 minutes
const MAX_CRASHES_IN_WINDOW: u32 = 3;
const HANG_TIMEOUT: Duration = Duration::from_secs(900); // 15 minutes
const MAX_CONSECUTIVE_ERRORS: u32 = 3;

/// Health status for an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    Healthy,
    Degraded,   // recovering, in backoff
    Failed,     // gave up restarting
}

/// Restart decision from the monitor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthAction {
    None,
    Restart,
    MarkFailed,
    KillAndRestart, // for hang detection
}

/// Per-agent health monitor.
pub struct HealthMonitor {
    status: HealthStatus,
    crash_times: Vec<Instant>,
    restart_count: u32,
    last_restart: Option<Instant>,
    busy_since: Option<Instant>,
}

impl HealthMonitor {
    pub fn new() -> Self {
        Self {
            status: HealthStatus::Healthy,
            crash_times: Vec::new(),
            restart_count: 0,
            last_restart: None,
            busy_since: None,
        }
    }

    pub fn status(&self) -> HealthStatus { self.status }
    pub fn restart_count(&self) -> u32 { self.restart_count }

    /// Calculate backoff duration: 5s × 2^(n-1), capped at 300s.
    pub fn backoff_duration(&self) -> Duration {
        if self.restart_count == 0 { return INITIAL_BACKOFF; }
        let secs = INITIAL_BACKOFF.as_secs() * (1u64 << (self.restart_count - 1).min(6));
        Duration::from_secs(secs.min(MAX_BACKOFF.as_secs()))
    }

    /// Called when agent state changes. Returns action to take.
    pub fn on_state_change(&mut self, state: AgentState, consecutive_errors: u32, now: Instant) -> HealthAction {
        match state {
            AgentState::Crashed => self.on_crash(now),
            AgentState::Busy => { self.busy_since = Some(now); HealthAction::None }
            AgentState::Ready | AgentState::Idle => {
                self.busy_since = None;
                if self.status == HealthStatus::Degraded {
                    self.status = HealthStatus::Healthy;
                    self.restart_count = 0;
                }
                HealthAction::None
            }
            AgentState::Errored => {
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

    /// Periodic check — call from daemon tick loop.
    pub fn tick(&mut self, current_state: AgentState, now: Instant) -> HealthAction {
        if self.status == HealthStatus::Failed { return HealthAction::None; }

        // Hang detection: Busy for too long
        if current_state == AgentState::Busy {
            if let Some(since) = self.busy_since {
                if now.duration_since(since) >= HANG_TIMEOUT {
                    self.busy_since = None;
                    return HealthAction::KillAndRestart;
                }
            }
        }

        // Check if backoff period elapsed for pending restart
        if current_state == AgentState::Crashed && self.status == HealthStatus::Degraded {
            if let Some(last) = self.last_restart {
                if now.duration_since(last) >= self.backoff_duration() {
                    return HealthAction::Restart;
                }
            }
        }

        HealthAction::None
    }

    /// Record restart attempt.
    pub fn on_restart(&mut self, now: Instant) {
        self.restart_count += 1;
        self.last_restart = Some(now);
        self.busy_since = None;
    }

    /// Reset health after successful manual intervention.
    pub fn reset(&mut self) {
        self.status = HealthStatus::Healthy;
        self.crash_times.clear();
        self.restart_count = 0;
        self.last_restart = None;
        self.busy_since = None;
    }

    fn on_crash(&mut self, now: Instant) -> HealthAction {
        // Prune old crashes outside window
        self.crash_times.retain(|t| now.duration_since(*t) < CRASH_WINDOW);
        self.crash_times.push(now);

        if self.crash_times.len() as u32 >= MAX_CRASHES_IN_WINDOW {
            self.status = HealthStatus::Failed;
            return HealthAction::MarkFailed;
        }

        self.status = HealthStatus::Degraded;
        if self.last_restart.is_none() {
            // First crash — restart immediately after initial backoff
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
        assert_eq!(hm.restart_count(), 0);
    }

    // ── Backoff calculation ─────────────────────────────────────────

    #[test]
    fn backoff_exponential() {
        let mut hm = HealthMonitor::new();
        assert_eq!(hm.backoff_duration(), Duration::from_secs(5));
        hm.restart_count = 1;
        assert_eq!(hm.backoff_duration(), Duration::from_secs(5));
        hm.restart_count = 2;
        assert_eq!(hm.backoff_duration(), Duration::from_secs(10));
        hm.restart_count = 3;
        assert_eq!(hm.backoff_duration(), Duration::from_secs(20));
        hm.restart_count = 4;
        assert_eq!(hm.backoff_duration(), Duration::from_secs(40));
    }

    #[test]
    fn backoff_capped_at_300s() {
        let mut hm = HealthMonitor::new();
        hm.restart_count = 10;
        assert_eq!(hm.backoff_duration(), Duration::from_secs(300));
        hm.restart_count = 100;
        assert_eq!(hm.backoff_duration(), Duration::from_secs(300));
    }

    // ── Crash window ────────────────────────────────────────────────

    #[test]
    fn single_crash_triggers_restart() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        let action = hm.on_state_change(AgentState::Crashed, 0, now);
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
        assert_eq!(hm.status(), HealthStatus::Failed);
    }

    #[test]
    fn old_crashes_outside_window_pruned() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.on_state_change(AgentState::Crashed, 0, now);
        hm.on_state_change(AgentState::Crashed, 0, now + Duration::from_secs(60));
        // Third crash after window expired (>10 min from first)
        let action = hm.on_state_change(AgentState::Crashed, 0, now + Duration::from_secs(700));
        assert_eq!(action, HealthAction::Restart); // not failed, old crashes pruned
        assert_eq!(hm.status(), HealthStatus::Degraded);
    }

    // ── Hang detection ──────────────────────────────────────────────

    #[test]
    fn hang_detected_after_timeout() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.on_state_change(AgentState::Busy, 0, now);
        // Not yet hung
        let action = hm.tick(AgentState::Busy, now + Duration::from_secs(600));
        assert_eq!(action, HealthAction::None);
        // Now hung (15 min)
        let action = hm.tick(AgentState::Busy, now + HANG_TIMEOUT);
        assert_eq!(action, HealthAction::KillAndRestart);
    }

    #[test]
    fn busy_reset_on_ready() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.on_state_change(AgentState::Busy, 0, now);
        hm.on_state_change(AgentState::Ready, 0, now + Duration::from_secs(60));
        // No hang even after long time
        let action = hm.tick(AgentState::Ready, now + HANG_TIMEOUT + Duration::from_secs(100));
        assert_eq!(action, HealthAction::None);
    }

    // ── Error loop detection ────────────────────────────────────────

    #[test]
    fn consecutive_errors_marks_failed() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        let action = hm.on_state_change(AgentState::Errored, 3, now);
        assert_eq!(action, HealthAction::MarkFailed);
        assert_eq!(hm.status(), HealthStatus::Failed);
    }

    #[test]
    fn fewer_errors_no_action() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        let action = hm.on_state_change(AgentState::Errored, 2, now);
        assert_eq!(action, HealthAction::None);
    }

    // ── Recovery ────────────────────────────────────────────────────

    #[test]
    fn ready_after_degraded_restores_healthy() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.on_state_change(AgentState::Crashed, 0, now);
        assert_eq!(hm.status(), HealthStatus::Degraded);
        hm.on_state_change(AgentState::Ready, 0, now + Duration::from_secs(10));
        assert_eq!(hm.status(), HealthStatus::Healthy);
        assert_eq!(hm.restart_count(), 0);
    }

    #[test]
    fn failed_state_no_tick_actions() {
        let mut hm = HealthMonitor::new();
        hm.status = HealthStatus::Failed;
        let action = hm.tick(AgentState::Crashed, Instant::now());
        assert_eq!(action, HealthAction::None);
    }

    #[test]
    fn restart_increments_count() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.on_restart(now);
        assert_eq!(hm.restart_count(), 1);
        hm.on_restart(now);
        assert_eq!(hm.restart_count(), 2);
    }

    #[test]
    fn reset_clears_everything() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.on_state_change(AgentState::Crashed, 0, now);
        hm.on_restart(now);
        hm.reset();
        assert_eq!(hm.status(), HealthStatus::Healthy);
        assert_eq!(hm.restart_count(), 0);
    }

    // ── Backoff-gated restart via tick ───────────────────────────────

    #[test]
    fn tick_triggers_restart_after_backoff() {
        let mut hm = HealthMonitor::new();
        let now = Instant::now();
        hm.on_state_change(AgentState::Crashed, 0, now);
        hm.on_restart(now);
        // Not enough time
        let action = hm.tick(AgentState::Crashed, now + Duration::from_secs(3));
        assert_eq!(action, HealthAction::None);
        // After backoff (5s)
        let action = hm.tick(AgentState::Crashed, now + Duration::from_secs(6));
        assert_eq!(action, HealthAction::Restart);
    }
}

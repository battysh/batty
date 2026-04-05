//! Proactive context-pressure tracking based on cumulative shim output volume.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::warn;

use super::super::*;

const WARNING_PERCENT: u64 = 70;
const NUDGE_PERCENT: u64 = 90;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextPressureAction {
    Warn,
    Nudge,
    Restart,
}

#[derive(Debug, Clone, Default)]
struct ContextPressureState {
    last_output_bytes: u64,
    warning_emitted: bool,
    nudge_sent: bool,
    over_threshold_since: Option<Instant>,
}

#[derive(Debug, Clone)]
pub(crate) struct ContextPressureTracker {
    members: HashMap<String, ContextPressureState>,
    threshold_bytes: u64,
    restart_delay: Duration,
}

impl Default for ContextPressureTracker {
    fn default() -> Self {
        Self::new(512_000, 120)
    }
}

impl ContextPressureTracker {
    pub(crate) fn new(threshold_bytes: u64, restart_delay_secs: u64) -> Self {
        Self {
            members: HashMap::new(),
            threshold_bytes: threshold_bytes.max(1),
            restart_delay: Duration::from_secs(restart_delay_secs),
        }
    }

    pub(super) fn clear_member(&mut self, member: &str) {
        self.members.remove(member);
    }

    pub(super) fn mark_not_working(&mut self, member: &str) {
        if let Some(state) = self.members.get_mut(member) {
            state.over_threshold_since = None;
        }
    }

    pub(super) fn observe_at(
        &mut self,
        member: &str,
        output_bytes: u64,
        is_working: bool,
        now: Instant,
    ) -> Vec<ContextPressureAction> {
        let warn_at = self.threshold_bytes.saturating_mul(WARNING_PERCENT) / 100;
        let nudge_at = self.threshold_bytes.saturating_mul(NUDGE_PERCENT) / 100;

        let state = self.members.entry(member.to_string()).or_default();
        if output_bytes < state.last_output_bytes {
            *state = ContextPressureState::default();
        }
        state.last_output_bytes = output_bytes;

        if !is_working {
            state.over_threshold_since = None;
            return Vec::new();
        }

        let mut actions = Vec::new();
        if output_bytes >= warn_at && !state.warning_emitted {
            state.warning_emitted = true;
            actions.push(ContextPressureAction::Warn);
        }
        if output_bytes >= nudge_at && !state.nudge_sent {
            state.nudge_sent = true;
            actions.push(ContextPressureAction::Nudge);
        }

        if output_bytes >= self.threshold_bytes {
            let over_threshold_since = state.over_threshold_since.get_or_insert(now);
            if now.duration_since(*over_threshold_since) >= self.restart_delay {
                actions.push(ContextPressureAction::Restart);
            }
        } else {
            state.over_threshold_since = None;
        }

        actions
    }
}

/// Minimum uptime (seconds) before a zero-output agent is considered dead.
const ZERO_OUTPUT_THRESHOLD_SECS: u64 = 600;

impl TeamDaemon {
    pub(in super::super) fn handle_context_pressure_stats(
        &mut self,
        member_name: &str,
        output_bytes: u64,
        uptime_secs: u64,
    ) -> Result<()> {
        // Detect dead agents: running for a long time with zero output bytes.
        // This catches agents that spawned but never produced any work.
        if output_bytes == 0 && uptime_secs >= ZERO_OUTPUT_THRESHOLD_SECS {
            if self.shim_handles.contains_key(member_name) {
                warn!(
                    member = %member_name,
                    uptime_secs,
                    "zero-output agent detected — restarting"
                );
                self.record_orchestrator_action(format!(
                    "health: restarting {} after {}s with zero output",
                    member_name, uptime_secs
                ));
                self.handle_shim_cold_respawn(member_name, "zero output")?;
                self.context_pressure_tracker.clear_member(member_name);
            }
            return Ok(());
        }

        let is_working = self.states.get(member_name) == Some(&MemberState::Working);
        let threshold_bytes = self
            .config
            .team_config
            .workflow_policy
            .context_pressure_threshold_bytes;
        let actions = self.context_pressure_tracker.observe_at(
            member_name,
            output_bytes,
            is_working,
            Instant::now(),
        );
        if actions.is_empty() {
            return Ok(());
        }

        let task_id = self.active_task_id(member_name);
        for action in actions {
            match action {
                ContextPressureAction::Warn => {
                    warn!(
                        member = %member_name,
                        task_id,
                        output_bytes,
                        threshold_bytes,
                        uptime_secs,
                        "detected rising context pressure"
                    );
                    self.record_context_pressure_warning(
                        member_name,
                        task_id,
                        output_bytes,
                        threshold_bytes,
                    );
                    self.record_orchestrator_action(format!(
                        "health: context pressure warning for {} ({}/{})",
                        member_name, output_bytes, threshold_bytes
                    ));
                }
                ContextPressureAction::Nudge => {
                    if let Some(member) = self
                        .config
                        .members
                        .iter()
                        .find(|member| member.name == member_name)
                        .cloned()
                    {
                        let message = self.prepend_member_nudge(
                            &member,
                            "Context pressure is high. Commit your current work if possible and wrap up before an automatic restart.",
                        );
                        if let Err(error) = self.queue_message("daemon", member_name, &message) {
                            warn!(
                                member = %member_name,
                                error = %error,
                                "failed to queue context-pressure nudge"
                            );
                        }
                    }
                    self.record_orchestrator_action(format!(
                        "health: nudged {} to wrap up under context pressure",
                        member_name
                    ));
                }
                ContextPressureAction::Restart => {
                    if !self.shim_handles.contains_key(member_name) {
                        continue;
                    }

                    self.handle_shim_cold_respawn(member_name, "context pressure")?;
                    if let Some(task_id) = task_id {
                        self.record_agent_restarted(
                            member_name,
                            task_id.to_string(),
                            "context_pressure",
                            1,
                        );
                    }
                    self.record_orchestrator_action(format!(
                        "health: restarted {} after sustained context pressure",
                        member_name
                    ));
                    self.context_pressure_tracker.clear_member(member_name);
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backdate_over_threshold(
        tracker: &mut ContextPressureTracker,
        member: &str,
        threshold_bytes: u64,
        secs_ago: u64,
    ) {
        let state = tracker.members.entry(member.to_string()).or_default();
        state.last_output_bytes = threshold_bytes;
        state.over_threshold_since = Some(Instant::now() - Duration::from_secs(secs_ago));
    }

    #[test]
    fn default_tracker_thresholds() {
        let tracker = ContextPressureTracker::default();
        assert_eq!(tracker.threshold_bytes, 512_000);
        assert_eq!(tracker.restart_delay, Duration::from_secs(120));
    }

    #[test]
    fn threshold_crossing_detection_warns_at_seventy_percent() {
        let mut tracker = ContextPressureTracker::new(1_000, 120);
        let actions = tracker.observe_at("eng-1", 700, true, Instant::now());
        assert_eq!(actions, vec![ContextPressureAction::Warn]);
    }

    #[test]
    fn graduated_response_levels_fire_in_order() {
        let mut tracker = ContextPressureTracker::new(1_000, 120);
        assert_eq!(
            tracker.observe_at("eng-1", 700, true, Instant::now()),
            vec![ContextPressureAction::Warn]
        );
        assert_eq!(
            tracker.observe_at("eng-1", 900, true, Instant::now()),
            vec![ContextPressureAction::Nudge]
        );
        backdate_over_threshold(&mut tracker, "eng-1", 1_000, 121);
        assert_eq!(
            tracker.observe_at("eng-1", 1_000, true, Instant::now()),
            vec![ContextPressureAction::Restart]
        );
    }

    #[test]
    fn counter_reset_on_restart_rewarns_after_output_drops() {
        let mut tracker = ContextPressureTracker::new(1_000, 120);
        assert_eq!(
            tracker.observe_at("eng-1", 900, true, Instant::now()),
            vec![ContextPressureAction::Warn, ContextPressureAction::Nudge]
        );
        assert!(
            tracker
                .observe_at("eng-1", 100, true, Instant::now())
                .is_empty()
        );
        assert_eq!(
            tracker.observe_at("eng-1", 700, true, Instant::now()),
            vec![ContextPressureAction::Warn]
        );
    }

    #[test]
    fn below_threshold_does_not_restart_without_delay() {
        let mut tracker = ContextPressureTracker::new(1_000, 120);
        let actions = tracker.observe_at("eng-1", 1_000, true, Instant::now());
        assert_eq!(
            actions,
            vec![ContextPressureAction::Warn, ContextPressureAction::Nudge]
        );
    }

    #[test]
    fn idle_member_clears_threshold_timer() {
        let mut tracker = ContextPressureTracker::new(1_000, 120);
        backdate_over_threshold(&mut tracker, "eng-1", 1_000, 121);
        assert!(
            tracker
                .observe_at("eng-1", 1_000, false, Instant::now())
                .is_empty()
        );
        assert!(
            tracker.members["eng-1"].over_threshold_since.is_none(),
            "idle transition should clear the restart timer"
        );
    }

    #[test]
    fn zero_output_threshold_is_ten_minutes() {
        assert_eq!(
            ZERO_OUTPUT_THRESHOLD_SECS, 600,
            "zero-output detection should trigger after 10 minutes"
        );
    }
}
